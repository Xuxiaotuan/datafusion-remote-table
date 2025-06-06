use crate::{
    Connection, ConnectionOptions, DFResult, RemoteSchemaRef, Transform, TransformStream,
    transform_schema,
};
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::common::Column;
use datafusion::common::tree_node::{Transformed, TreeNode};
use datafusion::execution::{SendableRecordBatchStream, TaskContext};
use datafusion::physical_expr::{EquivalenceProperties, Partitioning};
use datafusion::physical_plan::execution_plan::{Boundedness, EmissionType};
use datafusion::physical_plan::stream::RecordBatchStreamAdapter;
use datafusion::physical_plan::{
    DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties, project_schema,
};
use datafusion::prelude::Expr;
use futures::TryStreamExt;
use std::any::Any;
use std::sync::Arc;

#[derive(Debug)]
pub struct RemoteTableExec {
    pub(crate) conn_options: ConnectionOptions,
    pub(crate) sql: String,
    pub(crate) table_schema: SchemaRef,
    pub(crate) remote_schema: Option<RemoteSchemaRef>,
    pub(crate) projection: Option<Vec<usize>>,
    pub(crate) filters: Vec<Expr>,
    pub(crate) limit: Option<usize>,
    pub(crate) transform: Option<Arc<dyn Transform>>,
    conn: Arc<dyn Connection>,
    plan_properties: PlanProperties,
}

impl RemoteTableExec {
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        conn_options: ConnectionOptions,
        sql: String,
        table_schema: SchemaRef,
        remote_schema: Option<RemoteSchemaRef>,
        projection: Option<Vec<usize>>,
        filters: Vec<Expr>,
        limit: Option<usize>,
        transform: Option<Arc<dyn Transform>>,
        conn: Arc<dyn Connection>,
    ) -> DFResult<Self> {
        let transformed_table_schema = transform_schema(
            table_schema.clone(),
            transform.as_ref(),
            remote_schema.as_ref(),
        )?;
        let projected_schema = project_schema(&transformed_table_schema, projection.as_ref())?;
        let plan_properties = PlanProperties::new(
            EquivalenceProperties::new(projected_schema),
            Partitioning::UnknownPartitioning(1),
            EmissionType::Incremental,
            Boundedness::Bounded,
        );
        Ok(Self {
            conn_options,
            sql,
            table_schema,
            remote_schema,
            projection,
            filters,
            limit,
            transform,
            conn,
            plan_properties,
        })
    }
}

impl ExecutionPlan for RemoteTableExec {
    fn name(&self) -> &str {
        "RemoteTableExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.plan_properties
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DFResult<Arc<dyn ExecutionPlan>> {
        Ok(self)
    }

    fn execute(
        &self,
        partition: usize,
        _context: Arc<TaskContext>,
    ) -> DFResult<SendableRecordBatchStream> {
        assert_eq!(partition, 0);
        let schema = self.schema();
        let fut = build_and_transform_stream(
            self.conn.clone(),
            self.conn_options.clone(),
            self.sql.clone(),
            self.table_schema.clone(),
            self.remote_schema.clone(),
            self.projection.clone(),
            self.filters.clone(),
            self.limit,
            self.transform.clone(),
        );
        let stream = futures::stream::once(fut).try_flatten();
        Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
    }

    fn with_fetch(&self, limit: Option<usize>) -> Option<Arc<dyn ExecutionPlan>> {
        if self
            .conn_options
            .db_type()
            .support_rewrite_with_filters_limit(&self.sql)
        {
            Some(Arc::new(Self {
                conn_options: self.conn_options.clone(),
                sql: self.sql.clone(),
                table_schema: self.table_schema.clone(),
                remote_schema: self.remote_schema.clone(),
                projection: self.projection.clone(),
                filters: self.filters.clone(),
                limit,
                transform: self.transform.clone(),
                conn: self.conn.clone(),
                plan_properties: self.plan_properties.clone(),
            }))
        } else {
            None
        }
    }

    fn fetch(&self) -> Option<usize> {
        self.limit
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_and_transform_stream(
    conn: Arc<dyn Connection>,
    conn_options: ConnectionOptions,
    sql: String,
    table_schema: SchemaRef,
    remote_schema: Option<RemoteSchemaRef>,
    projection: Option<Vec<usize>>,
    filters: Vec<Expr>,
    limit: Option<usize>,
    transform: Option<Arc<dyn Transform>>,
) -> DFResult<SendableRecordBatchStream> {
    let transformed_table_schema = transform_schema(
        table_schema.clone(),
        transform.as_ref(),
        remote_schema.as_ref(),
    )?;

    let rewritten_filters =
        rewrite_filters_column(filters, &table_schema, &transformed_table_schema)?;

    let limit = if conn_options
        .db_type()
        .support_rewrite_with_filters_limit(&sql)
    {
        limit
    } else {
        None
    };

    let stream = conn
        .query(
            &conn_options,
            &sql,
            table_schema.clone(),
            projection.as_ref(),
            rewritten_filters.as_slice(),
            limit,
        )
        .await?;

    if let Some(transform) = transform.as_ref() {
        Ok(Box::pin(TransformStream::try_new(
            stream,
            transform.clone(),
            table_schema,
            projection,
            remote_schema,
        )?))
    } else {
        Ok(stream)
    }
}

fn rewrite_filters_column(
    filters: Vec<Expr>,
    table_schema: &SchemaRef,
    transformed_table_schema: &SchemaRef,
) -> DFResult<Vec<Expr>> {
    filters
        .into_iter()
        .map(|f| {
            f.transform_down(|e| {
                if let Expr::Column(col) = e {
                    let col_idx = transformed_table_schema.index_of(col.name())?;
                    let row_name = table_schema.field(col_idx).name().to_string();
                    Ok(Transformed::yes(Expr::Column(Column::new_unqualified(
                        row_name,
                    ))))
                } else {
                    Ok(Transformed::no(e))
                }
            })
            .map(|trans| trans.data)
        })
        .collect::<DFResult<Vec<_>>>()
}

impl DisplayAs for RemoteTableExec {
    fn fmt_as(&self, _t: DisplayFormatType, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "RemoteTableExec: limit={:?}, filters=[{}]",
            self.limit,
            self.filters
                .iter()
                .map(|e| format!("{e}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}
