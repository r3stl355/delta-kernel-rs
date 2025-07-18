use std::collections::HashSet;
use std::iter;
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::actions::SetTransaction;
use crate::actions::COMMIT_INFO_NAME;
use crate::actions::{get_log_add_schema, get_log_commit_info_schema, get_log_txn_schema};
use crate::error::Error;
use crate::expressions::{column_expr, Scalar, StructData};
use crate::path::ParsedLogPath;
use crate::schema::{MapType, SchemaRef, StructField, StructType};
use crate::snapshot::Snapshot;
use crate::{DataType, DeltaResult, Engine, EngineData, Expression, IntoEngineData, Version};

use url::Url;

const KERNEL_VERSION: &str = env!("CARGO_PKG_VERSION");
const UNKNOWN_OPERATION: &str = "UNKNOWN";

pub(crate) static ADD_FILES_SCHEMA: LazyLock<SchemaRef> = LazyLock::new(|| {
    Arc::new(StructType::new(vec![
        StructField::not_null("path", DataType::STRING),
        StructField::not_null(
            "partitionValues",
            MapType::new(DataType::STRING, DataType::STRING, true),
        ),
        StructField::not_null("size", DataType::LONG),
        StructField::not_null("modificationTime", DataType::LONG),
        StructField::not_null("dataChange", DataType::BOOLEAN),
    ]))
});

/// This function specifies the schema for the add_files metadata (and soon remove_files metadata).
/// Concretely, it is the expected schema for engine data passed to [`add_files`].
///
/// Each row represents metadata about a file to be added to the table.
///
/// [`add_files`]: crate::transaction::Transaction::add_files
pub fn add_files_schema() -> &'static SchemaRef {
    &ADD_FILES_SCHEMA
}

/// A transaction represents an in-progress write to a table. After creating a transaction, changes
/// to the table may be staged via the transaction methods before calling `commit` to commit the
/// changes to the table.
///
/// # Examples
///
/// ```rust,ignore
/// // create a transaction
/// let mut txn = table.new_transaction(&engine)?;
/// // stage table changes (right now only commit info)
/// txn.commit_info(Box::new(ArrowEngineData::new(engine_commit_info)));
/// // commit! (consume the transaction)
/// txn.commit(&engine)?;
/// ```
pub struct Transaction {
    read_snapshot: Arc<Snapshot>,
    operation: Option<String>,
    commit_info: Option<Arc<dyn EngineData>>,
    add_files_metadata: Vec<Box<dyn EngineData>>,
    // NB: hashmap would require either duplicating the appid or splitting SetTransaction
    // key/payload. HashSet requires Borrow<&str> with matching Eq, Ord, and Hash. Plus,
    // HashSet::insert drops the to-be-inserted value without returning the existing one, which
    // would make error messaging unnecessarily difficult. Thus, we keep Vec here and deduplicate in
    // the commit method.
    set_transactions: Vec<SetTransaction>,
    // commit-wide timestamp (in milliseconds since epoch) - used in ICT, `txn` action, etc. to
    // keep all timestamps within the same commit consistent.
    commit_timestamp: i64,
}

impl std::fmt::Debug for Transaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!(
            "Transaction {{ read_snapshot version: {}, commit_info: {} }}",
            self.read_snapshot.version(),
            self.commit_info.is_some()
        ))
    }
}

impl Transaction {
    /// Create a new transaction from a snapshot. The snapshot will be used to read the current
    /// state of the table (e.g. to read the current version).
    ///
    /// Instead of using this API, the more typical (user-facing) API is
    /// [Snapshot::transaction](crate::snapshot::Snapshot::transaction) to create a transaction from
    /// a snapshot.
    pub(crate) fn try_new(snapshot: impl Into<Arc<Snapshot>>) -> DeltaResult<Self> {
        let read_snapshot = snapshot.into();

        // important! before a read/write to the table we must check it is supported
        read_snapshot
            .table_configuration()
            .ensure_write_supported()?;

        // TODO: unify all these into a (safer) `fn current_time_ms()`
        let commit_timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .ok_or_else(|| Error::generic("Failed to get current time for commit_timestamp"))?;

        Ok(Transaction {
            read_snapshot,
            operation: None,
            commit_info: None,
            add_files_metadata: vec![],
            set_transactions: vec![],
            commit_timestamp,
        })
    }

    /// Consume the transaction and commit it to the table. The result is a [CommitResult] which
    /// will include the failed transaction in case of a conflict so the user can retry.
    pub fn commit(self, engine: &dyn Engine) -> DeltaResult<CommitResult> {
        // step 0: if there are txn(app_id, version) actions being committed, ensure that every
        // `app_id` is unique and create a row of `EngineData` for it.
        // TODO(zach): we currently do this in two passes - can we do it in one and still keep refs
        // in the HashSet?
        let mut app_ids = HashSet::new();
        if let Some(dup) = self
            .set_transactions
            .iter()
            .find(|t| !app_ids.insert(&t.app_id))
        {
            return Err(Error::generic(format!(
                "app_id {} already exists in transaction",
                dup.app_id
            )));
        }
        let set_transaction_actions = self
            .set_transactions
            .clone()
            .into_iter()
            .map(|txn| txn.into_engine_data(get_log_txn_schema().clone(), engine));

        // step one: construct the iterator of commit info + file actions we want to commit
        let engine_commit_info = self
            .commit_info
            .as_ref()
            .ok_or_else(|| Error::MissingCommitInfo)?;
        let commit_info_actions = generate_commit_info(
            engine,
            self.operation.as_deref(),
            self.commit_timestamp,
            engine_commit_info.as_ref(),
        );
        let add_actions = generate_adds(engine, self.add_files_metadata.iter().map(|a| a.as_ref()));

        let actions = iter::once(commit_info_actions)
            .chain(add_actions)
            .chain(set_transaction_actions);

        // step two: set new commit version (current_version + 1) and path to write
        let commit_version = self.read_snapshot.version() + 1;
        let commit_path =
            ParsedLogPath::new_commit(self.read_snapshot.table_root(), commit_version)?;

        // step three: commit the actions as a json file in the log
        let json_handler = engine.json_handler();
        match json_handler.write_json_file(&commit_path.location, Box::new(actions), false) {
            Ok(()) => Ok(CommitResult::Committed(commit_version)),
            Err(Error::FileAlreadyExists(_)) => Ok(CommitResult::Conflict(self, commit_version)),
            Err(e) => Err(e),
        }
    }

    /// Set the operation that this transaction is performing. This string will be persisted in the
    /// commit and visible to anyone who describes the table history.
    pub fn with_operation(mut self, operation: String) -> Self {
        self.operation = Some(operation);
        self
    }

    /// Include a SetTransaction (app_id and version) action for this transaction (with an optional
    /// `last_updated` timestamp).
    /// Note that each app_id can only appear once per transaction. That is, multiple app_ids with
    /// different versions are disallowed in a single transaction. If a duplicate app_id is
    /// included, the `commit` will fail (that is, we don't eagerly check app_id validity here).
    pub fn with_transaction_id(mut self, app_id: String, version: i64) -> Self {
        let set_transaction = SetTransaction::new(app_id, version, Some(self.commit_timestamp));
        self.set_transactions.push(set_transaction);
        self
    }

    /// WARNING: This is an unstable API and will likely change in the future.
    ///
    /// Add commit info to the transaction. This is commit-wide metadata that is written as the
    /// first action in the commit. The engine data passed here must have exactly one row, and we
    /// only read one column: `engineCommitInfo` which must be a map<string, string> encoding the
    /// metadata.
    ///
    /// The engine is required to provide commit info before committing the transaction. If the
    /// engine would like to omit engine-specific commit info, it can do so by passing pass a
    /// commit_info engine data chunk with one row and one column of type `Map<string, string>`
    /// that can either be `null` or contain an empty map.
    ///
    /// Any other columns in the data chunk are ignored.
    pub fn with_commit_info(mut self, commit_info: Box<dyn EngineData>) -> Self {
        self.commit_info = Some(commit_info.into());
        self
    }

    // Generate the logical-to-physical transform expression which must be evaluated on every data
    // chunk before writing. At the moment, this is a transaction-wide expression.
    fn generate_logical_to_physical(&self) -> Expression {
        // for now, we just pass through all the columns except partition columns.
        // note this is _incorrect_ if table config deems we need partition columns.
        let partition_columns = &self.read_snapshot.metadata().partition_columns;
        let schema = self.read_snapshot.schema();
        let fields = schema
            .fields()
            .filter(|f| !partition_columns.contains(f.name()))
            .map(|f| Expression::column([f.name()]));
        Expression::struct_from(fields)
    }

    /// Get the write context for this transaction. At the moment, this is constant for the whole
    /// transaction.
    // Note: after we introduce metadata updates (modify table schema, etc.), we need to make sure
    // that engines cannot call this method after a metadata change, since the write context could
    // have invalid metadata.
    pub fn get_write_context(&self) -> WriteContext {
        let target_dir = self.read_snapshot.table_root();
        let snapshot_schema = self.read_snapshot.schema();
        let logical_to_physical = self.generate_logical_to_physical();
        WriteContext::new(target_dir.clone(), snapshot_schema, logical_to_physical)
    }

    /// Add files to include in this transaction. This API generally enables the engine to
    /// add/append/insert data (files) to the table. Note that this API can be called multiple times
    /// to add multiple batches.
    ///
    /// The expected schema for `add_metadata` is given by [`add_files_schema`].
    pub fn add_files(&mut self, add_metadata: Box<dyn EngineData>) {
        self.add_files_metadata.push(add_metadata);
    }
}

// convert add_files_metadata into add actions using an expression to transform the data in a single
// pass
fn generate_adds<'a>(
    engine: &dyn Engine,
    add_files_metadata: impl Iterator<Item = &'a dyn EngineData> + Send + 'a,
) -> impl Iterator<Item = DeltaResult<Box<dyn EngineData>>> + Send + 'a {
    let evaluation_handler = engine.evaluation_handler();
    let add_files_schema = add_files_schema();
    let log_schema = get_log_add_schema();

    add_files_metadata.map(move |add_files_batch| {
        let adds_expr = Expression::struct_from([Expression::struct_from(
            add_files_schema
                .fields()
                .map(|f| Expression::column([f.name()])),
        )]);
        let adds_evaluator = evaluation_handler.new_expression_evaluator(
            add_files_schema.clone(),
            adds_expr,
            log_schema.clone().into(),
        );
        adds_evaluator.evaluate(add_files_batch)
    })
}

/// WriteContext is data derived from a [`Transaction`] that can be provided to writers in order to
/// write table data.
///
/// [`Transaction`]: struct.Transaction.html
pub struct WriteContext {
    target_dir: Url,
    schema: SchemaRef,
    logical_to_physical: Expression,
}

impl WriteContext {
    fn new(target_dir: Url, schema: SchemaRef, logical_to_physical: Expression) -> Self {
        WriteContext {
            target_dir,
            schema,
            logical_to_physical,
        }
    }

    pub fn target_dir(&self) -> &Url {
        &self.target_dir
    }

    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    pub fn logical_to_physical(&self) -> &Expression {
        &self.logical_to_physical
    }
}

/// Result after committing a transaction. If 'committed', the version is the new version written
/// to the log. If 'conflict', the transaction is returned so the caller can resolve the conflict
/// (along with the version which conflicted).
// TODO(zach): in order to make the returning of a transaction useful, we need to add APIs to
// update the transaction to a new version etc.
#[derive(Debug)]
pub enum CommitResult {
    /// The transaction was successfully committed at the version.
    Committed(Version),
    /// This transaction conflicted with an existing version (at the version given).
    Conflict(Transaction, Version),
}

// given the engine's commit info we want to create commitInfo action to commit (and append more actions to)
fn generate_commit_info(
    engine: &dyn Engine,
    operation: Option<&str>,
    timestamp: i64,
    engine_commit_info: &dyn EngineData,
) -> DeltaResult<Box<dyn EngineData>> {
    if engine_commit_info.len() != 1 {
        return Err(Error::InvalidCommitInfo(format!(
            "Engine commit info should have exactly one row, found {}",
            engine_commit_info.len()
        )));
    }

    let commit_info_exprs = [
        Expression::literal(timestamp),
        Expression::literal(operation.unwrap_or(UNKNOWN_OPERATION)),
        // HACK (part 1/2): since we don't have proper map support, we create a literal struct with
        // one null field to create data that serializes as "operationParameters": {}
        Expression::literal(Scalar::Struct(StructData::try_new(
            vec![StructField::nullable(
                "operation_parameter_int",
                DataType::INTEGER,
            )],
            vec![Scalar::Null(DataType::INTEGER)],
        )?)),
        Expression::literal(format!("v{KERNEL_VERSION}")),
        column_expr!("engineCommitInfo"),
    ];
    let commit_info_expr = Expression::struct_from([Expression::struct_from(commit_info_exprs)]);
    let commit_info_schema = get_log_commit_info_schema().as_ref();

    // HACK (part 2/2): we need to modify the commit info schema to match the expression above (a
    // struct with a single null int field).
    let mut commit_info_empty_struct_schema = commit_info_schema.clone();
    let commit_info_field = commit_info_empty_struct_schema
        .fields
        .get_mut(COMMIT_INFO_NAME)
        .ok_or_else(|| Error::missing_column(COMMIT_INFO_NAME))?;
    let DataType::Struct(mut commit_info_data_type) = commit_info_field.data_type().clone() else {
        return Err(Error::internal_error(
            "commit_info_field should be a struct",
        ));
    };
    let engine_commit_info_schema =
        commit_info_data_type.project_as_struct(&["engineCommitInfo"])?;
    let hack_data_type = DataType::Struct(Box::new(StructType::new(vec![StructField::nullable(
        "hack_operation_parameter_int",
        DataType::INTEGER,
    )])));

    commit_info_data_type
        .fields
        .get_mut("operationParameters")
        .ok_or_else(|| Error::missing_column("operationParameters"))?
        .data_type = hack_data_type;

    // Since writing in-commit timestamps is not supported, we remove the field so it is not
    // written to the log
    commit_info_data_type
        .fields
        .shift_remove("inCommitTimestamp");
    commit_info_field.data_type = DataType::Struct(commit_info_data_type);

    let commit_info_evaluator = engine.evaluation_handler().new_expression_evaluator(
        engine_commit_info_schema.into(),
        commit_info_expr,
        commit_info_empty_struct_schema.into(),
    );

    commit_info_evaluator.evaluate(engine_commit_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::engine::arrow_data::ArrowEngineData;
    use crate::engine::arrow_expression::ArrowEvaluationHandler;
    use crate::schema::MapType;
    use crate::{EvaluationHandler, JsonHandler, ParquetHandler, StorageHandler};

    use crate::arrow::array::{MapArray, MapBuilder, MapFieldNames, StringArray, StringBuilder};
    use crate::arrow::datatypes::{DataType as ArrowDataType, Field, Schema as ArrowSchema};
    use crate::arrow::error::ArrowError;
    use crate::arrow::json::writer::LineDelimitedWriter;
    use crate::arrow::record_batch::RecordBatch;

    struct ExprEngine(Arc<dyn EvaluationHandler>);

    impl ExprEngine {
        fn new() -> Self {
            ExprEngine(Arc::new(ArrowEvaluationHandler))
        }
    }

    impl Engine for ExprEngine {
        fn evaluation_handler(&self) -> Arc<dyn EvaluationHandler> {
            self.0.clone()
        }

        fn json_handler(&self) -> Arc<dyn JsonHandler> {
            unimplemented!()
        }

        fn parquet_handler(&self) -> Arc<dyn ParquetHandler> {
            unimplemented!()
        }

        fn storage_handler(&self) -> Arc<dyn StorageHandler> {
            unimplemented!()
        }
    }

    fn build_map(entries: Vec<(&str, &str)>) -> MapArray {
        let key_builder = StringBuilder::new();
        let val_builder = StringBuilder::new();
        let names = MapFieldNames {
            entry: "entries".to_string(),
            key: "key".to_string(),
            value: "value".to_string(),
        };
        let mut builder = MapBuilder::new(Some(names), key_builder, val_builder);
        for (key, val) in entries {
            builder.keys().append_value(key);
            builder.values().append_value(val);
            builder.append(true).unwrap();
        }
        builder.finish()
    }

    // convert it to JSON just for ease of comparison (and since we ultimately persist as JSON)
    fn as_json(data: Box<dyn EngineData>) -> serde_json::Value {
        let record_batch: RecordBatch = data
            .into_any()
            .downcast::<ArrowEngineData>()
            .unwrap()
            .into();

        let buf = Vec::new();
        let mut writer = LineDelimitedWriter::new(buf);
        writer.write_batches(&[&record_batch]).unwrap();
        writer.finish().unwrap();
        let buf = writer.into_inner();

        serde_json::from_slice(&buf).unwrap()
    }

    #[test]
    fn test_generate_commit_info() -> DeltaResult<()> {
        let engine = ExprEngine::new();
        let engine_commit_info_schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "engineCommitInfo",
            ArrowDataType::Map(
                Arc::new(Field::new(
                    "entries",
                    ArrowDataType::Struct(
                        vec![
                            Field::new("key", ArrowDataType::Utf8, false),
                            Field::new("value", ArrowDataType::Utf8, true),
                        ]
                        .into(),
                    ),
                    false,
                )),
                false,
            ),
            false,
        )]));

        let map_array = build_map(vec![("engineInfo", "default engine")]);
        let commit_info_batch =
            RecordBatch::try_new(engine_commit_info_schema, vec![Arc::new(map_array)])?;

        let actions = generate_commit_info(
            &engine,
            Some("test operation"),
            123456789,
            &ArrowEngineData::new(commit_info_batch),
        )?;

        let expected = serde_json::json!({
            "commitInfo": {
                "timestamp": 123456789,
                "operation": "test operation",
                "kernelVersion": format!("v{}", env!("CARGO_PKG_VERSION")),
                "operationParameters": {},
                "engineCommitInfo": {
                    "engineInfo": "default engine"
                }
            }
        });

        assert_eq!(actions.len(), 1);
        let result = as_json(actions);
        assert_eq!(result, expected);

        Ok(())
    }

    #[test]
    fn test_commit_info_with_multiple_columns() -> DeltaResult<()> {
        let engine = ExprEngine::new();
        let engine_commit_info_schema = Arc::new(ArrowSchema::new(vec![
            Field::new(
                "engineCommitInfo",
                ArrowDataType::Map(
                    Arc::new(Field::new(
                        "entries",
                        ArrowDataType::Struct(
                            vec![
                                Field::new("key", ArrowDataType::Utf8, false),
                                Field::new("value", ArrowDataType::Utf8, true),
                            ]
                            .into(),
                        ),
                        false,
                    )),
                    false,
                ),
                false,
            ),
            Field::new("operation", ArrowDataType::Utf8, true),
        ]));

        let map_array = build_map(vec![("engineInfo", "default engine")]);

        let commit_info_batch = RecordBatch::try_new(
            engine_commit_info_schema,
            vec![
                Arc::new(map_array),
                Arc::new(StringArray::from(vec!["some_string"])),
            ],
        )?;

        let actions = generate_commit_info(
            &engine,
            Some("test operation"),
            123456789,
            &ArrowEngineData::new(commit_info_batch),
        )?;

        let expected = serde_json::json!({
            "commitInfo": {
                "timestamp": 123456789,
                "operation": "test operation",
                "kernelVersion": format!("v{}", env!("CARGO_PKG_VERSION")),
                "operationParameters": {},
                "engineCommitInfo": {
                    "engineInfo": "default engine"
                }
            }
        });

        assert_eq!(actions.len(), 1);
        let result = as_json(actions);
        assert_eq!(result, expected);

        Ok(())
    }

    #[test]
    fn test_invalid_commit_info_missing_column() -> DeltaResult<()> {
        let engine = ExprEngine::new();
        let engine_commit_info_schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "some_column_name",
            ArrowDataType::Utf8,
            true,
        )]));
        let commit_info_batch = RecordBatch::try_new(
            engine_commit_info_schema,
            vec![Arc::new(StringArray::new_null(1))],
        )?;

        let _ = generate_commit_info(
            &engine,
            Some("test operation"),
            123456789,
            &ArrowEngineData::new(commit_info_batch),
        )
        .map_err(|e| match e {
            Error::Arrow(ArrowError::SchemaError(_)) => (),
            Error::Backtraced { source, .. }
                if matches!(&*source, Error::Arrow(ArrowError::SchemaError(_))) => {}
            _ => panic!("expected arrow schema error error, got {e:?}"),
        });

        Ok(())
    }

    #[test]
    fn test_invalid_commit_info_invalid_column_type() -> DeltaResult<()> {
        let engine = ExprEngine::new();
        let engine_commit_info_schema = Arc::new(ArrowSchema::new(vec![Field::new(
            "engineCommitInfo",
            ArrowDataType::Utf8,
            true,
        )]));
        let commit_info_batch = RecordBatch::try_new(
            engine_commit_info_schema,
            vec![Arc::new(StringArray::new_null(1))],
        )?;

        let _ = generate_commit_info(
            &engine,
            Some("test operation"),
            123456789,
            &ArrowEngineData::new(commit_info_batch),
        )
        .map_err(|e| match e {
            Error::Arrow(ArrowError::InvalidArgumentError(_)) => (),
            Error::Backtraced { source, .. }
                if matches!(&*source, Error::Arrow(ArrowError::InvalidArgumentError(_))) => {}
            _ => panic!("expected arrow invalid arg error, got {e:?}"),
        });

        Ok(())
    }

    fn assert_empty_commit_info(
        data: Box<dyn EngineData>,
        write_engine_commit_info: bool,
        timestamp: i64,
    ) -> DeltaResult<()> {
        assert_eq!(data.len(), 1);
        let expected = if write_engine_commit_info {
            serde_json::json!({
                "commitInfo": {
                    "timestamp": timestamp,
                    "operation": "test operation",
                    "kernelVersion": format!("v{}", env!("CARGO_PKG_VERSION")),
                    "operationParameters": {},
                    "engineCommitInfo": {}
                }
            })
        } else {
            serde_json::json!({
                "commitInfo": {
                    "timestamp": timestamp,
                    "operation": "test operation",
                    "kernelVersion": format!("v{}", env!("CARGO_PKG_VERSION")),
                    "operationParameters": {},
                }
            })
        };
        let result = as_json(data);
        assert_eq!(result, expected);
        Ok(())
    }

    // Three cases for empty commit info:
    // 1. `engineCommitInfo` column with an empty Map<string, string>
    // 2. `engineCommitInfo` null column of type Map<string, string>
    // 3. a column that has a name other than `engineCommitInfo`; Delta can detect that the column
    //    is missing and substitute a null literal in its place. The type of that column doesn't
    //    matter, Delta will ignore it.
    #[test]
    fn test_empty_commit_info() -> DeltaResult<()> {
        // test with null map and empty map
        for is_null in [true, false] {
            let engine = ExprEngine::new();
            let engine_commit_info_schema = Arc::new(ArrowSchema::new(vec![Field::new(
                "engineCommitInfo",
                ArrowDataType::Map(
                    Arc::new(Field::new(
                        "entries",
                        ArrowDataType::Struct(
                            vec![
                                Field::new("key", ArrowDataType::Utf8, false),
                                Field::new("value", ArrowDataType::Utf8, true),
                            ]
                            .into(),
                        ),
                        false,
                    )),
                    false,
                ),
                true,
            )]));

            let key_builder = StringBuilder::new();
            let val_builder = StringBuilder::new();
            let names = crate::arrow::array::MapFieldNames {
                entry: "entries".to_string(),
                key: "key".to_string(),
                value: "value".to_string(),
            };
            let mut builder =
                crate::arrow::array::MapBuilder::new(Some(names), key_builder, val_builder);
            builder.append(is_null).unwrap();
            let array = builder.finish();

            let commit_info_batch =
                RecordBatch::try_new(engine_commit_info_schema, vec![Arc::new(array)])?;

            let timestamp = 123456;
            let actions = generate_commit_info(
                &engine,
                Some("test operation"),
                timestamp,
                &ArrowEngineData::new(commit_info_batch),
            )?;

            assert_empty_commit_info(actions, is_null, timestamp)?;
        }
        Ok(())
    }

    #[test]
    fn test_add_files_schema() {
        let schema = add_files_schema();
        let expected = StructType::new(vec![
            StructField::not_null("path", DataType::STRING),
            StructField::not_null(
                "partitionValues",
                MapType::new(DataType::STRING, DataType::STRING, true),
            ),
            StructField::not_null("size", DataType::LONG),
            StructField::not_null("modificationTime", DataType::LONG),
            StructField::not_null("dataChange", DataType::BOOLEAN),
        ]);
        assert_eq!(*schema, expected.into());
    }
}
