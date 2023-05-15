// Copyright 2023 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use common_catalog::table_context::TableContext;
use common_exception::ErrorCode;
use common_exception::Result;
use common_expression::ComputedExpr;
use common_expression::DataBlock;
use common_expression::DataSchemaRef;
use common_expression::Expr;
use common_sql::evaluator::BlockOperator;
use common_sql::evaluator::CompoundBlockOperator;
use common_sql::parse_computed_exprs;

use crate::pipelines::processors::port::InputPort;
use crate::pipelines::processors::port::OutputPort;
use crate::pipelines::processors::processor::ProcessorPtr;
use crate::pipelines::processors::transforms::transform::Transform;
use crate::pipelines::processors::transforms::transform::Transformer;
use crate::sessions::QueryContext;

pub struct TransformAddComputedColumns {
    expression_transform: CompoundBlockOperator,
    input_len: usize,
}

impl TransformAddComputedColumns
where Self: Transform
{
    pub fn try_create(
        ctx: Arc<QueryContext>,
        input: Arc<InputPort>,
        output: Arc<OutputPort>,
        input_schema: DataSchemaRef,
        output_schema: DataSchemaRef,
    ) -> Result<ProcessorPtr> {
        let mut exprs = Vec::with_capacity(output_schema.fields().len());
        for f in output_schema.fields().iter() {
            let expr = if !input_schema.has_field(f.name()) {
                if let Some(ComputedExpr::Stored(stored_expr)) = f.computed_expr() {
                    let mut expr =
                        parse_computed_exprs(ctx.clone(), input_schema.clone(), stored_expr)?;
                    let mut expr = expr.remove(0);
                    if expr.data_type() != f.data_type() {
                        expr = Expr::Cast {
                            span: None,
                            is_try: f.data_type().is_nullable(),
                            expr: Box::new(expr),
                            dest_type: f.data_type().clone(),
                        };
                    }
                    expr
                } else {
                    return Err(ErrorCode::Internal(
                        "Missed field must be a computed column",
                    ));
                }
            } else {
                let field = input_schema.field_with_name(f.name()).unwrap();
                let id = input_schema.index_of(f.name()).unwrap();
                Expr::ColumnRef {
                    span: None,
                    id,
                    data_type: field.data_type().clone(),
                    display_name: field.name().clone(),
                }
            };
            exprs.push(expr);
        }

        let func_ctx = ctx.get_function_context()?;
        let expression_transform = CompoundBlockOperator {
            ctx: func_ctx,
            operators: vec![BlockOperator::Map { exprs }],
        };

        Ok(ProcessorPtr::create(Transformer::create(
            input,
            output,
            Self {
                expression_transform,
                input_len: input_schema.num_fields(),
            },
        )))
    }
}

impl Transform for TransformAddComputedColumns {
    const NAME: &'static str = "AddComputedColumnsTransform";

    fn transform(&mut self, mut block: DataBlock) -> Result<DataBlock> {
        block = self.expression_transform.transform(block)?;
        let columns = block.columns()[self.input_len..].to_owned();
        Ok(DataBlock::new(columns, block.num_rows()))
    }
}
