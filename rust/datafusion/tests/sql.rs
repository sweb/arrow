// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::convert::TryFrom;
use std::sync::Arc;

extern crate arrow;
extern crate datafusion;

use arrow::{array::*, datatypes::TimeUnit};
use arrow::{datatypes::Int32Type, datatypes::Int64Type, record_batch::RecordBatch};
use arrow::{
    datatypes::{DataType, Field, Schema, SchemaRef},
    util::display::array_value_to_string,
};

use datafusion::execution::context::ExecutionContext;
use datafusion::logical_plan::{LogicalPlan, ToDFSchema};
use datafusion::prelude::create_udf;
use datafusion::{
    datasource::{csv::CsvReadOptions, MemTable},
    physical_plan::collect,
};
use datafusion::{error::Result, physical_plan::ColumnarValue};

#[tokio::test]
async fn nyc() -> Result<()> {
    // schema for nyxtaxi csv files
    let schema = Schema::new(vec![
        Field::new("VendorID", DataType::Utf8, true),
        Field::new("tpep_pickup_datetime", DataType::Utf8, true),
        Field::new("tpep_dropoff_datetime", DataType::Utf8, true),
        Field::new("passenger_count", DataType::Utf8, true),
        Field::new("trip_distance", DataType::Float64, true),
        Field::new("RatecodeID", DataType::Utf8, true),
        Field::new("store_and_fwd_flag", DataType::Utf8, true),
        Field::new("PULocationID", DataType::Utf8, true),
        Field::new("DOLocationID", DataType::Utf8, true),
        Field::new("payment_type", DataType::Utf8, true),
        Field::new("fare_amount", DataType::Float64, true),
        Field::new("extra", DataType::Float64, true),
        Field::new("mta_tax", DataType::Float64, true),
        Field::new("tip_amount", DataType::Float64, true),
        Field::new("tolls_amount", DataType::Float64, true),
        Field::new("improvement_surcharge", DataType::Float64, true),
        Field::new("total_amount", DataType::Float64, true),
    ]);

    let mut ctx = ExecutionContext::new();
    ctx.register_csv(
        "tripdata",
        "file.csv",
        CsvReadOptions::new().schema(&schema),
    )?;

    let logical_plan = ctx.create_logical_plan(
        "SELECT passenger_count, MIN(fare_amount), MAX(fare_amount) \
         FROM tripdata GROUP BY passenger_count",
    )?;

    let optimized_plan = ctx.optimize(&logical_plan)?;

    match &optimized_plan {
        LogicalPlan::Aggregate { input, .. } => match input.as_ref() {
            LogicalPlan::TableScan {
                ref projected_schema,
                ..
            } => {
                assert_eq!(2, projected_schema.fields().len());
                assert_eq!(projected_schema.field(0).name(), "passenger_count");
                assert_eq!(projected_schema.field(1).name(), "fare_amount");
            }
            _ => unreachable!(),
        },
        _ => unreachable!(false),
    }

    Ok(())
}

#[tokio::test]
async fn parquet_query() {
    let mut ctx = ExecutionContext::new();
    register_alltypes_parquet(&mut ctx);
    // NOTE that string_col is actually a binary column and does not have the UTF8 logical type
    // so we need an explicit cast
    let sql = "SELECT id, CAST(string_col AS varchar) FROM alltypes_plain";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["4", "0"],
        vec!["5", "1"],
        vec!["6", "0"],
        vec!["7", "1"],
        vec!["2", "0"],
        vec!["3", "1"],
        vec!["0", "0"],
        vec!["1", "1"],
    ];

    assert_eq!(expected, actual);
}

#[tokio::test]
async fn parquet_single_nan_schema() {
    let mut ctx = ExecutionContext::new();
    let testdata = arrow::util::test_util::parquet_test_data();
    ctx.register_parquet("single_nan", &format!("{}/single_nan.parquet", testdata))
        .unwrap();
    let sql = "SELECT mycol FROM single_nan";
    let plan = ctx.create_logical_plan(&sql).unwrap();
    let plan = ctx.optimize(&plan).unwrap();
    let plan = ctx.create_physical_plan(&plan).unwrap();
    let results = collect(plan).await.unwrap();
    for batch in results {
        assert_eq!(1, batch.num_rows());
        assert_eq!(1, batch.num_columns());
    }
}

#[tokio::test]
#[ignore = "Test ignored, will be enabled as part of the nested Parquet reader"]
async fn parquet_list_columns() {
    let mut ctx = ExecutionContext::new();
    let testdata = arrow::util::test_util::parquet_test_data();
    ctx.register_parquet(
        "list_columns",
        &format!("{}/list_columns.parquet", testdata),
    )
    .unwrap();

    let schema = Arc::new(Schema::new(vec![
        Field::new(
            "int64_list",
            DataType::List(Box::new(Field::new("item", DataType::Int64, true))),
            true,
        ),
        Field::new(
            "utf8_list",
            DataType::List(Box::new(Field::new("item", DataType::Utf8, true))),
            true,
        ),
    ]));

    let sql = "SELECT int64_list, utf8_list FROM list_columns";
    let plan = ctx.create_logical_plan(&sql).unwrap();
    let plan = ctx.optimize(&plan).unwrap();
    let plan = ctx.create_physical_plan(&plan).unwrap();
    let results = collect(plan).await.unwrap();

    //   int64_list              utf8_list
    // 0  [1, 2, 3]        [abc, efg, hij]
    // 1  [None, 1]                   None
    // 2        [4]  [efg, None, hij, xyz]

    assert_eq!(1, results.len());
    let batch = &results[0];
    assert_eq!(3, batch.num_rows());
    assert_eq!(2, batch.num_columns());
    assert_eq!(schema, batch.schema());

    let int_list_array = batch
        .column(0)
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap();
    let utf8_list_array = batch
        .column(1)
        .as_any()
        .downcast_ref::<ListArray>()
        .unwrap();

    assert_eq!(
        int_list_array
            .value(0)
            .as_any()
            .downcast_ref::<PrimitiveArray<Int64Type>>()
            .unwrap(),
        &PrimitiveArray::<Int64Type>::from(vec![Some(1), Some(2), Some(3),])
    );

    assert_eq!(
        utf8_list_array
            .value(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap(),
        &StringArray::try_from(vec![Some("abc"), Some("efg"), Some("hij"),]).unwrap()
    );

    assert_eq!(
        int_list_array
            .value(1)
            .as_any()
            .downcast_ref::<PrimitiveArray<Int64Type>>()
            .unwrap(),
        &PrimitiveArray::<Int64Type>::from(vec![None, Some(1),])
    );

    assert!(utf8_list_array.is_null(1));

    assert_eq!(
        int_list_array
            .value(2)
            .as_any()
            .downcast_ref::<PrimitiveArray<Int64Type>>()
            .unwrap(),
        &PrimitiveArray::<Int64Type>::from(vec![Some(4),])
    );

    let result = utf8_list_array.value(2);
    let result = result.as_any().downcast_ref::<StringArray>().unwrap();

    assert_eq!(result.value(0), "efg");
    assert!(result.is_null(1));
    assert_eq!(result.value(2), "hij");
    assert_eq!(result.value(3), "xyz");
}

#[tokio::test]
async fn csv_select_nested() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT o1, o2, c3
               FROM (
                 SELECT c1 AS o1, c2 + 1 AS o2, c3
                 FROM (
                   SELECT c1, c2, c3, c4
                   FROM aggregate_test_100
                   WHERE c1 = 'a' AND c2 >= 4
                   ORDER BY c2 ASC, c3 ASC
                 )
               )";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["a", "5", "-101"],
        vec!["a", "5", "-54"],
        vec!["a", "5", "-38"],
        vec!["a", "5", "65"],
        vec!["a", "6", "-101"],
        vec!["a", "6", "-31"],
        vec!["a", "6", "36"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_count_star() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT COUNT(*), COUNT(1) AS c, COUNT(c1) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["100", "100", "100"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_with_predicate() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, c12 FROM aggregate_test_100 WHERE c12 > 0.376 AND c12 < 0.4";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["e", "0.39144436569161134"],
        vec!["d", "0.38870280983958583"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_with_negative_predicate() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, c4 FROM aggregate_test_100 WHERE c3 < -55 AND -c4 > 30000";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["e", "-31500"], vec!["c", "-30187"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_with_negated_predicate() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT COUNT(1) FROM aggregate_test_100 WHERE NOT(c1 != 'a')";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["21"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_with_is_not_null_predicate() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT COUNT(1) FROM aggregate_test_100 WHERE c1 IS NOT NULL";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["100"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_with_is_null_predicate() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT COUNT(1) FROM aggregate_test_100 WHERE c1 IS NULL";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["0"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_int_min_max() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c2, MIN(c12), MAX(c12) FROM aggregate_test_100 GROUP BY c2";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["1", "0.05636955101974106", "0.9965400387585364"],
        vec!["2", "0.16301110515739792", "0.991517828651004"],
        vec!["3", "0.047343434291126085", "0.9293883502480845"],
        vec!["4", "0.02182578039211991", "0.9237877978193884"],
        vec!["5", "0.01479305307777301", "0.9723580396501548"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_float32() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_simple_csv(&mut ctx)?;

    let sql =
        "SELECT COUNT(*) as cnt, c1 FROM aggregate_simple GROUP BY c1 ORDER BY cnt DESC";
    let actual = execute(&mut ctx, sql).await;

    let expected = vec![
        vec!["5", "0.00005"],
        vec!["4", "0.00004"],
        vec!["3", "0.00003"],
        vec!["2", "0.00002"],
        vec!["1", "0.00001"],
    ];
    assert_eq!(expected, actual);

    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_float64() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_simple_csv(&mut ctx)?;

    let sql =
        "SELECT COUNT(*) as cnt, c2 FROM aggregate_simple GROUP BY c2 ORDER BY cnt DESC";
    let actual = execute(&mut ctx, sql).await;

    let expected = vec![
        vec!["5", "0.000000000005"],
        vec!["4", "0.000000000004"],
        vec!["3", "0.000000000003"],
        vec!["2", "0.000000000002"],
        vec!["1", "0.000000000001"],
    ];
    assert_eq!(expected, actual);

    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_boolean() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_simple_csv(&mut ctx)?;

    let sql =
        "SELECT COUNT(*) as cnt, c3 FROM aggregate_simple GROUP BY c3 ORDER BY cnt DESC";
    let actual = execute(&mut ctx, sql).await;

    let expected = vec![vec!["9", "true"], vec!["6", "false"]];
    assert_eq!(expected, actual);

    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_two_columns() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, c2, MIN(c3) FROM aggregate_test_100 GROUP BY c1, c2";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["a", "1", "-85"],
        vec!["a", "2", "-48"],
        vec!["a", "3", "-72"],
        vec!["a", "4", "-101"],
        vec!["a", "5", "-101"],
        vec!["b", "1", "12"],
        vec!["b", "2", "-60"],
        vec!["b", "3", "-101"],
        vec!["b", "4", "-117"],
        vec!["b", "5", "-82"],
        vec!["c", "1", "-24"],
        vec!["c", "2", "-117"],
        vec!["c", "3", "-2"],
        vec!["c", "4", "-90"],
        vec!["c", "5", "-94"],
        vec!["d", "1", "-99"],
        vec!["d", "2", "93"],
        vec!["d", "3", "-76"],
        vec!["d", "4", "5"],
        vec!["d", "5", "-59"],
        vec!["e", "1", "36"],
        vec!["e", "2", "-61"],
        vec!["e", "3", "-95"],
        vec!["e", "4", "-56"],
        vec!["e", "5", "-86"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_and_having() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, MIN(c3) AS m FROM aggregate_test_100 GROUP BY c1 HAVING m < -100 AND MAX(c3) > 70";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![vec!["a", "-101"], vec!["c", "-117"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_and_having_and_where() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, MIN(c3) AS m
               FROM aggregate_test_100
               WHERE c1 IN ('a', 'b')
               GROUP BY c1
               HAVING m < -100 AND MAX(c3) > 70";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![vec!["a", "-101"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_having_without_group_by() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, c2, c3 FROM aggregate_test_100 HAVING c2 >= 4 AND c3 > 90";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["c", "4", "123"],
        vec!["c", "5", "118"],
        vec!["d", "4", "102"],
        vec!["e", "4", "96"],
        vec!["e", "4", "97"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_avg_sqrt() -> Result<()> {
    let mut ctx = create_ctx()?;
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT avg(custom_sqrt(c12)) FROM aggregate_test_100";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![vec!["0.6706002946036462"]];
    assert_float_eq(&expected, &actual);
    Ok(())
}

/// test that casting happens on udfs.
/// c11 is f32, but `custom_sqrt` requires f64. Casting happens but the logical plan and
/// physical plan have the same schema.
#[tokio::test]
async fn csv_query_custom_udf_with_cast() -> Result<()> {
    let mut ctx = create_ctx()?;
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT avg(custom_sqrt(c11)) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["0.6584408483418833"]];
    assert_float_eq(&expected, &actual);
    Ok(())
}

/// sqrt(f32) is slightly different than sqrt(CAST(f32 AS double)))
#[tokio::test]
async fn sqrt_f32_vs_f64() -> Result<()> {
    let mut ctx = create_ctx()?;
    register_aggregate_csv(&mut ctx)?;
    // sqrt(f32)'s plan passes
    let sql = "SELECT avg(sqrt(c11)) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["0.6584408485889435"]];

    assert_eq!(actual, expected);
    let sql = "SELECT avg(sqrt(CAST(c11 AS double))) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["0.6584408483418833"]];
    assert_float_eq(&expected, &actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_error() -> Result<()> {
    // sin(utf8) should error
    let mut ctx = create_ctx()?;
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT sin(c1) FROM aggregate_test_100";
    let plan = ctx.create_logical_plan(&sql);
    assert!(plan.is_err());
    Ok(())
}

// this query used to deadlock due to the call udf(udf())
#[tokio::test]
async fn csv_query_sqrt_sqrt() -> Result<()> {
    let mut ctx = create_ctx()?;
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT sqrt(sqrt(c12)) FROM aggregate_test_100 LIMIT 1";
    let actual = execute(&mut ctx, sql).await;
    // sqrt(sqrt(c12=0.9294097332465232)) = 0.9818650561397431
    let expected = vec![vec!["0.9818650561397431"]];
    assert_float_eq(&expected, &actual);
    Ok(())
}

#[allow(clippy::unnecessary_wraps)]
fn create_ctx() -> Result<ExecutionContext> {
    let mut ctx = ExecutionContext::new();

    // register a custom UDF
    ctx.register_udf(create_udf(
        "custom_sqrt",
        vec![DataType::Float64],
        Arc::new(DataType::Float64),
        Arc::new(custom_sqrt),
    ));

    Ok(ctx)
}

fn custom_sqrt(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    let arg = &args[0];
    if let ColumnarValue::Array(v) = arg {
        let input = v
            .as_any()
            .downcast_ref::<Float64Array>()
            .expect("cast failed");

        let array: Float64Array = input.iter().map(|v| v.map(|x| x.sqrt())).collect();
        Ok(ColumnarValue::Array(Arc::new(array)))
    } else {
        unimplemented!()
    }
}

#[tokio::test]
async fn csv_query_avg() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT avg(c12) FROM aggregate_test_100";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![vec!["0.5089725099127211"]];
    assert_float_eq(&expected, &actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_avg() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, avg(c12) FROM aggregate_test_100 GROUP BY c1";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["a", "0.48754517466109415"],
        vec!["b", "0.41040709263815384"],
        vec!["c", "0.6600456536439784"],
        vec!["d", "0.48855379387549824"],
        vec!["e", "0.48600669271341534"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_avg_with_projection() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT avg(c12), c1 FROM aggregate_test_100 GROUP BY c1";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["0.41040709263815384", "b"],
        vec!["0.48600669271341534", "e"],
        vec!["0.48754517466109415", "a"],
        vec!["0.48855379387549824", "d"],
        vec!["0.6600456536439784", "c"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_avg_multi_batch() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT avg(c12) FROM aggregate_test_100";
    let plan = ctx.create_logical_plan(&sql).unwrap();
    let plan = ctx.optimize(&plan).unwrap();
    let plan = ctx.create_physical_plan(&plan).unwrap();
    let results = collect(plan).await.unwrap();
    let batch = &results[0];
    let column = batch.column(0);
    let array = column.as_any().downcast_ref::<Float64Array>().unwrap();
    let actual = array.value(0);
    let expected = 0.5089725;
    // Due to float number's accuracy, different batch size will lead to different
    // answers.
    assert!((expected - actual).abs() < 0.01);
    Ok(())
}

#[tokio::test]
async fn csv_query_nullif_divide_by_0() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c8/nullif(c7, 0) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let actual = &actual[80..90]; // We just want to compare rows 80-89
    let expected = vec![
        vec!["258"],
        vec!["664"],
        vec!["NULL"],
        vec!["22"],
        vec!["164"],
        vec!["448"],
        vec!["365"],
        vec!["1640"],
        vec!["671"],
        vec!["203"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_count() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT count(c12) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["100"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_int_count() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, count(c12) FROM aggregate_test_100 GROUP BY c1";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["a", "21"],
        vec!["b", "19"],
        vec!["c", "21"],
        vec!["d", "18"],
        vec!["e", "21"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_with_aliased_aggregate() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, count(c12) AS count FROM aggregate_test_100 GROUP BY c1";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["a", "21"],
        vec!["b", "19"],
        vec!["c", "21"],
        vec!["d", "18"],
        vec!["e", "21"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_group_by_string_min_max() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1, MIN(c12), MAX(c12) FROM aggregate_test_100 GROUP BY c1";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![
        vec!["a", "0.02182578039211991", "0.9800193410444061"],
        vec!["b", "0.04893135681998029", "0.9185813970744787"],
        vec!["c", "0.0494924465469434", "0.991517828651004"],
        vec!["d", "0.061029375346466685", "0.9748360509016578"],
        vec!["e", "0.01479305307777301", "0.9965400387585364"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_cast() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT CAST(c12 AS float) FROM aggregate_test_100 WHERE c12 > 0.376 AND c12 < 0.4";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["0.39144436569161134"], vec!["0.38870280983958583"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_cast_literal() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql =
        "SELECT c12, CAST(1 AS float) FROM aggregate_test_100 WHERE c12 > CAST(0 AS float) LIMIT 2";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["0.9294097332465232", "1"],
        vec!["0.3114712539863804", "1"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_limit() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1 FROM aggregate_test_100 LIMIT 2";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["c"], vec!["d"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_limit_bigger_than_nbr_of_rows() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c2 FROM aggregate_test_100 LIMIT 200";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["2"],
        vec!["5"],
        vec!["1"],
        vec!["1"],
        vec!["5"],
        vec!["4"],
        vec!["3"],
        vec!["3"],
        vec!["1"],
        vec!["4"],
        vec!["1"],
        vec!["4"],
        vec!["3"],
        vec!["2"],
        vec!["1"],
        vec!["1"],
        vec!["2"],
        vec!["1"],
        vec!["3"],
        vec!["2"],
        vec!["4"],
        vec!["1"],
        vec!["5"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["4"],
        vec!["5"],
        vec!["2"],
        vec!["3"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["5"],
        vec!["3"],
        vec!["1"],
        vec!["2"],
        vec!["3"],
        vec!["3"],
        vec!["3"],
        vec!["2"],
        vec!["4"],
        vec!["1"],
        vec!["3"],
        vec!["2"],
        vec!["5"],
        vec!["2"],
        vec!["1"],
        vec!["4"],
        vec!["1"],
        vec!["4"],
        vec!["2"],
        vec!["5"],
        vec!["4"],
        vec!["2"],
        vec!["3"],
        vec!["4"],
        vec!["4"],
        vec!["4"],
        vec!["5"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["2"],
        vec!["4"],
        vec!["2"],
        vec!["3"],
        vec!["5"],
        vec!["1"],
        vec!["1"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["2"],
        vec!["1"],
        vec!["1"],
        vec!["5"],
        vec!["4"],
        vec!["5"],
        vec!["2"],
        vec!["3"],
        vec!["2"],
        vec!["4"],
        vec!["1"],
        vec!["3"],
        vec!["4"],
        vec!["3"],
        vec!["2"],
        vec!["5"],
        vec!["3"],
        vec!["3"],
        vec!["2"],
        vec!["5"],
        vec!["5"],
        vec!["4"],
        vec!["1"],
        vec!["3"],
        vec!["3"],
        vec!["4"],
        vec!["4"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_limit_with_same_nbr_of_rows() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c2 FROM aggregate_test_100 LIMIT 100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["2"],
        vec!["5"],
        vec!["1"],
        vec!["1"],
        vec!["5"],
        vec!["4"],
        vec!["3"],
        vec!["3"],
        vec!["1"],
        vec!["4"],
        vec!["1"],
        vec!["4"],
        vec!["3"],
        vec!["2"],
        vec!["1"],
        vec!["1"],
        vec!["2"],
        vec!["1"],
        vec!["3"],
        vec!["2"],
        vec!["4"],
        vec!["1"],
        vec!["5"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["4"],
        vec!["5"],
        vec!["2"],
        vec!["3"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["5"],
        vec!["3"],
        vec!["1"],
        vec!["2"],
        vec!["3"],
        vec!["3"],
        vec!["3"],
        vec!["2"],
        vec!["4"],
        vec!["1"],
        vec!["3"],
        vec!["2"],
        vec!["5"],
        vec!["2"],
        vec!["1"],
        vec!["4"],
        vec!["1"],
        vec!["4"],
        vec!["2"],
        vec!["5"],
        vec!["4"],
        vec!["2"],
        vec!["3"],
        vec!["4"],
        vec!["4"],
        vec!["4"],
        vec!["5"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["2"],
        vec!["4"],
        vec!["2"],
        vec!["3"],
        vec!["5"],
        vec!["1"],
        vec!["1"],
        vec!["4"],
        vec!["2"],
        vec!["1"],
        vec!["2"],
        vec!["1"],
        vec!["1"],
        vec!["5"],
        vec!["4"],
        vec!["5"],
        vec!["2"],
        vec!["3"],
        vec!["2"],
        vec!["4"],
        vec!["1"],
        vec!["3"],
        vec!["4"],
        vec!["3"],
        vec!["2"],
        vec!["5"],
        vec!["3"],
        vec!["3"],
        vec!["2"],
        vec!["5"],
        vec!["5"],
        vec!["4"],
        vec!["1"],
        vec!["3"],
        vec!["3"],
        vec!["4"],
        vec!["4"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_limit_zero() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c1 FROM aggregate_test_100 LIMIT 0";
    let actual = execute(&mut ctx, sql).await;
    let expected: Vec<Vec<String>> = vec![];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_create_external_table() {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "SELECT c1, c2, c3, c4, c5, c6, c7, c8, c9, 10, c11, c12, c13 FROM aggregate_test_100 LIMIT 1";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec![
        "c",
        "2",
        "1",
        "18109",
        "2033001162",
        "-6513304855495910254",
        "25",
        "43062",
        "1491205016",
        "10",
        "0.110830784",
        "0.9294097332465232",
        "6WfVFBVGJSQb7FhA7E0lBwdvjfZnSW",
    ]];
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn csv_query_external_table_count() {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "SELECT COUNT(c12) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["100"]];
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn csv_query_external_table_sum() {
    let mut ctx = ExecutionContext::new();
    // cast smallint and int to bigint to avoid overflow during calculation
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql =
        "SELECT SUM(CAST(c7 AS BIGINT)), SUM(CAST(c8 AS BIGINT)) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["13060", "3017641"]];
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn csv_query_count_star() {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "SELECT COUNT(*) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["100"]];
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn csv_query_count_one() {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "SELECT COUNT(1) FROM aggregate_test_100";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["100"]];
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn case_when() -> Result<()> {
    let mut ctx = create_case_context()?;
    let sql = "SELECT \
        CASE WHEN c1 = 'a' THEN 1 \
             WHEN c1 = 'b' THEN 2 \
             END \
        FROM t1";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["1"], vec!["2"], vec!["NULL"], vec!["NULL"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn case_when_else() -> Result<()> {
    let mut ctx = create_case_context()?;
    let sql = "SELECT \
        CASE WHEN c1 = 'a' THEN 1 \
             WHEN c1 = 'b' THEN 2 \
             ELSE 999 END \
        FROM t1";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["1"], vec!["2"], vec!["999"], vec!["999"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn case_when_with_base_expr() -> Result<()> {
    let mut ctx = create_case_context()?;
    let sql = "SELECT \
        CASE c1 WHEN 'a' THEN 1 \
             WHEN 'b' THEN 2 \
             END \
        FROM t1";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["1"], vec!["2"], vec!["NULL"], vec!["NULL"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn case_when_else_with_base_expr() -> Result<()> {
    let mut ctx = create_case_context()?;
    let sql = "SELECT \
        CASE c1 WHEN 'a' THEN 1 \
             WHEN 'b' THEN 2 \
             ELSE 999 END \
        FROM t1";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["1"], vec!["2"], vec!["999"], vec!["999"]];
    assert_eq!(expected, actual);
    Ok(())
}

fn create_case_context() -> Result<ExecutionContext> {
    let mut ctx = ExecutionContext::new();
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Utf8, true)]));
    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec![
            Some("a"),
            Some("b"),
            Some("c"),
            None,
        ]))],
    )?;
    let table = MemTable::try_new(schema, vec![vec![data]])?;
    ctx.register_table("t1", Arc::new(table));
    Ok(ctx)
}

#[tokio::test]
async fn equijoin() -> Result<()> {
    let mut ctx = create_join_context("t1_id", "t2_id")?;
    let sql =
        "SELECT t1_id, t1_name, t2_name FROM t1 JOIN t2 ON t1_id = t2_id ORDER BY t1_id";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["11", "a", "z"],
        vec!["22", "b", "y"],
        vec!["44", "d", "x"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn left_join() -> Result<()> {
    let mut ctx = create_join_context("t1_id", "t2_id")?;
    let sql = "SELECT t1_id, t1_name, t2_name FROM t1 LEFT JOIN t2 ON t1_id = t2_id ORDER BY t1_id";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["11", "a", "z"],
        vec!["22", "b", "y"],
        vec!["33", "c", "NULL"],
        vec!["44", "d", "x"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn right_join() -> Result<()> {
    let mut ctx = create_join_context("t1_id", "t2_id")?;
    let sql =
        "SELECT t1_id, t1_name, t2_name FROM t1 RIGHT JOIN t2 ON t1_id = t2_id ORDER BY t1_id";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["NULL", "NULL", "w"],
        vec!["11", "a", "z"],
        vec!["22", "b", "y"],
        vec!["44", "d", "x"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn left_join_using() -> Result<()> {
    let mut ctx = create_join_context("id", "id")?;
    let sql = "SELECT id, t1_name, t2_name FROM t1 LEFT JOIN t2 USING (id) ORDER BY id";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["11", "a", "z"],
        vec!["22", "b", "y"],
        vec!["33", "c", "NULL"],
        vec!["44", "d", "x"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn equijoin_implicit_syntax() -> Result<()> {
    let mut ctx = create_join_context("t1_id", "t2_id")?;
    let sql =
        "SELECT t1_id, t1_name, t2_name FROM t1, t2 WHERE t1_id = t2_id ORDER BY t1_id";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["11", "a", "z"],
        vec!["22", "b", "y"],
        vec!["44", "d", "x"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn equijoin_implicit_syntax_with_filter() -> Result<()> {
    let mut ctx = create_join_context("t1_id", "t2_id")?;
    let sql = "SELECT t1_id, t1_name, t2_name \
        FROM t1, t2 \
        WHERE t1_id > 0 \
        AND t1_id = t2_id \
        AND t2_id < 99 \
        ORDER BY t1_id";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["11", "a", "z"],
        vec!["22", "b", "y"],
        vec!["44", "d", "x"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn equijoin_implicit_syntax_reversed() -> Result<()> {
    let mut ctx = create_join_context("t1_id", "t2_id")?;
    let sql =
        "SELECT t1_id, t1_name, t2_name FROM t1, t2 WHERE t2_id = t1_id ORDER BY t1_id";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["11", "a", "z"],
        vec!["22", "b", "y"],
        vec!["44", "d", "x"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn cartesian_join() -> Result<()> {
    let ctx = create_join_context("t1_id", "t2_id")?;
    let sql = "SELECT t1_id, t1_name, t2_name FROM t1, t2 ORDER BY t1_id";
    let maybe_plan = ctx.create_logical_plan(&sql);
    assert_eq!(
        "This feature is not implemented: Cartesian joins are not supported",
        &format!("{}", maybe_plan.err().unwrap())
    );
    Ok(())
}

fn create_join_context(
    column_left: &str,
    column_right: &str,
) -> Result<ExecutionContext> {
    let mut ctx = ExecutionContext::new();

    let t1_schema = Arc::new(Schema::new(vec![
        Field::new(column_left, DataType::UInt32, true),
        Field::new("t1_name", DataType::Utf8, true),
    ]));
    let t1_data = RecordBatch::try_new(
        t1_schema.clone(),
        vec![
            Arc::new(UInt32Array::from(vec![11, 22, 33, 44])),
            Arc::new(StringArray::from(vec![
                Some("a"),
                Some("b"),
                Some("c"),
                Some("d"),
            ])),
        ],
    )?;
    let t1_table = MemTable::try_new(t1_schema, vec![vec![t1_data]])?;
    ctx.register_table("t1", Arc::new(t1_table));

    let t2_schema = Arc::new(Schema::new(vec![
        Field::new(column_right, DataType::UInt32, true),
        Field::new("t2_name", DataType::Utf8, true),
    ]));
    let t2_data = RecordBatch::try_new(
        t2_schema.clone(),
        vec![
            Arc::new(UInt32Array::from(vec![11, 22, 44, 55])),
            Arc::new(StringArray::from(vec![
                Some("z"),
                Some("y"),
                Some("x"),
                Some("w"),
            ])),
        ],
    )?;
    let t2_table = MemTable::try_new(t2_schema, vec![vec![t2_data]])?;
    ctx.register_table("t2", Arc::new(t2_table));

    Ok(ctx)
}

fn create_join_context_qualified() -> Result<ExecutionContext> {
    let mut ctx = ExecutionContext::new();

    let t1_schema = Arc::new(Schema::new(vec![
        Field::new("a", DataType::UInt32, true),
        Field::new("b", DataType::UInt32, true),
        Field::new("c", DataType::UInt32, true),
    ]));
    let t1_data = RecordBatch::try_new(
        t1_schema.clone(),
        vec![
            Arc::new(UInt32Array::from(vec![1, 2, 3, 4])),
            Arc::new(UInt32Array::from(vec![10, 20, 30, 40])),
            Arc::new(UInt32Array::from(vec![50, 60, 70, 80])),
        ],
    )?;
    let t1_table = MemTable::try_new(t1_schema, vec![vec![t1_data]])?;
    ctx.register_table("t1", Arc::new(t1_table));

    let t2_schema = Arc::new(Schema::new(vec![
        Field::new("a", DataType::UInt32, true),
        Field::new("b", DataType::UInt32, true),
        Field::new("c", DataType::UInt32, true),
    ]));
    let t2_data = RecordBatch::try_new(
        t2_schema.clone(),
        vec![
            Arc::new(UInt32Array::from(vec![1, 2, 9, 4])),
            Arc::new(UInt32Array::from(vec![100, 200, 300, 400])),
            Arc::new(UInt32Array::from(vec![500, 600, 700, 800])),
        ],
    )?;
    let t2_table = MemTable::try_new(t2_schema, vec![vec![t2_data]])?;
    ctx.register_table("t2", Arc::new(t2_table));

    Ok(ctx)
}

#[tokio::test]
async fn csv_explain() {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "EXPLAIN SELECT c1 FROM aggregate_test_100 where c2 > 10";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec![
            "logical_plan",
            "Projection: #c1\n  Filter: #c2 Gt Int64(10)\n    TableScan: aggregate_test_100 projection=None"
        ]
    ];
    assert_eq!(expected, actual);

    // Also, expect same result with lowercase explain
    let sql = "explain SELECT c1 FROM aggregate_test_100 where c2 > 10";
    let actual = execute(&mut ctx, sql).await;
    assert_eq!(expected, actual);
}

#[tokio::test]
async fn csv_explain_verbose() {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "EXPLAIN VERBOSE SELECT c1 FROM aggregate_test_100 where c2 > 10";
    let actual = execute(&mut ctx, sql).await;

    // flatten to a single string
    let actual = actual.into_iter().map(|r| r.join("\t")).collect::<String>();

    // Don't actually test the contents of the debuging output (as
    // that may change and keeping this test updated will be a
    // pain). Instead just check for a few key pieces.
    assert!(actual.contains("logical_plan"), "Actual: '{}'", actual);
    assert!(actual.contains("physical_plan"), "Actual: '{}'", actual);
    assert!(actual.contains("#c2 Gt Int64(10)"), "Actual: '{}'", actual);
}

fn aggr_test_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("c1", DataType::Utf8, false),
        Field::new("c2", DataType::UInt32, false),
        Field::new("c3", DataType::Int8, false),
        Field::new("c4", DataType::Int16, false),
        Field::new("c5", DataType::Int32, false),
        Field::new("c6", DataType::Int64, false),
        Field::new("c7", DataType::UInt8, false),
        Field::new("c8", DataType::UInt16, false),
        Field::new("c9", DataType::UInt32, false),
        Field::new("c10", DataType::UInt64, false),
        Field::new("c11", DataType::Float32, false),
        Field::new("c12", DataType::Float64, false),
        Field::new("c13", DataType::Utf8, false),
    ]))
}

async fn register_aggregate_csv_by_sql(ctx: &mut ExecutionContext) {
    let testdata = arrow::util::test_util::arrow_test_data();

    // TODO: The following c9 should be migrated to UInt32 and c10 should be UInt64 once
    // unsigned is supported.
    let df = ctx
        .sql(&format!(
            "
    CREATE EXTERNAL TABLE aggregate_test_100 (
        c1  VARCHAR NOT NULL,
        c2  INT NOT NULL,
        c3  SMALLINT NOT NULL,
        c4  SMALLINT NOT NULL,
        c5  INT NOT NULL,
        c6  BIGINT NOT NULL,
        c7  SMALLINT NOT NULL,
        c8  INT NOT NULL,
        c9  BIGINT NOT NULL,
        c10 VARCHAR NOT NULL,
        c11 FLOAT NOT NULL,
        c12 DOUBLE NOT NULL,
        c13 VARCHAR NOT NULL
    )
    STORED AS CSV
    WITH HEADER ROW
    LOCATION '{}/csv/aggregate_test_100.csv'
    ",
            testdata
        ))
        .expect("Creating dataframe for CREATE EXTERNAL TABLE");

    // Mimic the CLI and execute the resulting plan -- even though it
    // is effectively a no-op (returns zero rows)
    let results = df.collect().await.expect("Executing CREATE EXTERNAL TABLE");
    assert!(
        results.is_empty(),
        "Expected no rows from executing CREATE EXTERNAL TABLE"
    );
}

fn register_aggregate_csv(ctx: &mut ExecutionContext) -> Result<()> {
    let testdata = arrow::util::test_util::arrow_test_data();
    let schema = aggr_test_schema();
    ctx.register_csv(
        "aggregate_test_100",
        &format!("{}/csv/aggregate_test_100.csv", testdata),
        CsvReadOptions::new().schema(&schema),
    )?;
    Ok(())
}

fn register_aggregate_simple_csv(ctx: &mut ExecutionContext) -> Result<()> {
    // It's not possible to use aggregate_test_100, not enought similar values to test grouping on floats
    let schema = Arc::new(Schema::new(vec![
        Field::new("c1", DataType::Float32, false),
        Field::new("c2", DataType::Float64, false),
        Field::new("c3", DataType::Boolean, false),
    ]));

    ctx.register_csv(
        "aggregate_simple",
        "tests/aggregate_simple.csv",
        CsvReadOptions::new().schema(&schema),
    )?;
    Ok(())
}

fn register_alltypes_parquet(ctx: &mut ExecutionContext) {
    let testdata = arrow::util::test_util::parquet_test_data();
    ctx.register_parquet(
        "alltypes_plain",
        &format!("{}/alltypes_plain.parquet", testdata),
    )
    .unwrap();
}

/// Execute query and return result set as 2-d table of Vecs
/// `result[row][column]`
async fn execute(ctx: &mut ExecutionContext, sql: &str) -> Vec<Vec<String>> {
    let msg = format!("Creating logical plan for '{}'", sql);
    let plan = ctx.create_logical_plan(&sql).expect(&msg);
    let logical_schema = plan.schema();

    let msg = format!("Optimizing logical plan for '{}': {:?}", sql, plan);
    let plan = ctx.optimize(&plan).expect(&msg);
    let optimized_logical_schema = plan.schema();

    let msg = format!("Creating physical plan for '{}': {:?}", sql, plan);
    let plan = ctx.create_physical_plan(&plan).expect(&msg);
    let physical_schema = plan.schema();

    let msg = format!("Executing physical plan for '{}': {:?}", sql, plan);
    let results = collect(plan).await.expect(&msg);

    assert_eq!(logical_schema.as_ref(), optimized_logical_schema.as_ref());
    assert_eq!(
        logical_schema.as_ref(),
        &physical_schema.to_dfschema().unwrap()
    );

    result_vec(&results)
}

/// Specialised String representation
fn col_str(column: &ArrayRef, row_index: usize) -> String {
    if column.is_null(row_index) {
        return "NULL".to_string();
    }

    // Special case ListArray as there is no pretty print support for it yet
    if let DataType::FixedSizeList(_, n) = column.data_type() {
        let array = column
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap()
            .value(row_index);

        let mut r = Vec::with_capacity(*n as usize);
        for i in 0..*n {
            r.push(col_str(&array, i as usize));
        }
        return format!("[{}]", r.join(","));
    }

    array_value_to_string(column, row_index)
        .ok()
        .unwrap_or_else(|| "???".to_string())
}

/// Converts the results into a 2d array of strings, `result[row][column]`
/// Special cases nulls to NULL for testing
fn result_vec(results: &[RecordBatch]) -> Vec<Vec<String>> {
    let mut result = vec![];
    for batch in results {
        for row_index in 0..batch.num_rows() {
            let row_vec = batch
                .columns()
                .iter()
                .map(|column| col_str(column, row_index))
                .collect();
            result.push(row_vec);
        }
    }
    result
}

async fn generic_query_length<T: 'static + Array + From<Vec<&'static str>>>(
    datatype: DataType,
) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", datatype, false)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(T::from(vec!["", "a", "aa", "aaa"]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT length(c1) FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["0"], vec!["1"], vec!["2"], vec!["3"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_length() -> Result<()> {
    generic_query_length::<StringArray>(DataType::Utf8).await
}

#[tokio::test]
async fn query_large_length() -> Result<()> {
    generic_query_length::<LargeStringArray>(DataType::LargeUtf8).await
}

#[tokio::test]
async fn query_not() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Boolean, true)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(BooleanArray::from(vec![
            Some(false),
            None,
            Some(true),
        ]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT NOT c1 FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["true"], vec!["NULL"], vec!["false"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_concat() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("c1", DataType::Utf8, false),
        Field::new("c2", DataType::Int32, true),
    ]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["", "a", "aa", "aaa"])),
            Arc::new(Int32Array::from(vec![Some(0), Some(1), None, Some(3)])),
        ],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT concat(c1, '-hi-', cast(c2 as varchar)) FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["-hi-0"],
        vec!["a-hi-1"],
        vec!["aa-hi-"],
        vec!["aaa-hi-3"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_array() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("c1", DataType::Utf8, false),
        Field::new("c2", DataType::Int32, true),
    ]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec!["", "a", "aa", "aaa"])),
            Arc::new(Int32Array::from(vec![Some(0), Some(1), None, Some(3)])),
        ],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT array(c1, cast(c2 as varchar)) FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["[,0]"],
        vec!["[a,1]"],
        vec!["[aa,NULL]"],
        vec!["[aaa,3]"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_query_sum_cast() {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    // c8 = i32; c9 = i64
    let sql = "SELECT c8 + c9 FROM aggregate_test_100";
    // check that the physical and logical schemas are equal
    execute(&mut ctx, sql).await;
}

#[tokio::test]
async fn query_where_neg_num() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;

    // Negative numbers do not parse correctly as of Arrow 2.0.0
    let sql = "select c7, c8 from aggregate_test_100 where c7 >= -2 and c7 < 10";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["7", "45465"],
        vec!["5", "40622"],
        vec!["0", "61069"],
        vec!["2", "20120"],
        vec!["4", "39363"],
    ];
    assert_eq!(expected, actual);

    // Also check floating point neg numbers
    let sql = "select c7, c8 from aggregate_test_100 where c7 >= -2.9 and c7 < 10";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["7", "45465"],
        vec!["5", "40622"],
        vec!["0", "61069"],
        vec!["2", "20120"],
        vec!["4", "39363"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn like() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "SELECT COUNT(c1) FROM aggregate_test_100 WHERE c13 LIKE '%FB%'";
    // check that the physical and logical schemas are equal
    let actual = execute(&mut ctx, sql).await;

    let expected = vec![vec!["1"]];
    assert_eq!(expected, actual);
    Ok(())
}

fn make_timestamp_nano_table() -> Result<Arc<MemTable>> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("ts", DataType::Timestamp(TimeUnit::Nanosecond, None), false),
        Field::new("value", DataType::Int32, true),
    ]));

    let mut builder = TimestampNanosecondArray::builder(3);

    builder.append_value(1599572549190855000)?; // 2020-09-08T13:42:29.190855+00:00
    builder.append_value(1599568949190855000)?; // 2020-09-08T12:42:29.190855+00:00
    builder.append_value(1599565349190855000)?; // 2020-09-08T11:42:29.190855+00:00

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(builder.finish()),
            Arc::new(Int32Array::from(vec![Some(1), Some(2), Some(3)])),
        ],
    )?;
    let table = MemTable::try_new(schema, vec![vec![data]])?;
    Ok(Arc::new(table))
}

#[tokio::test]
async fn to_timestamp() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    ctx.register_table("ts_data", make_timestamp_nano_table()?);

    let sql = "SELECT COUNT(*) FROM ts_data where ts > to_timestamp('2020-09-08T12:00:00+00:00')";
    let actual = execute(&mut ctx, sql).await;

    let expected = vec![vec!["2"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_is_null() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Float64, true)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Float64Array::from(vec![
            Some(1.0),
            None,
            Some(f64::NAN),
        ]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT c1 IS NULL FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["false"], vec!["true"], vec!["false"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_is_not_null() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Float64, true)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Float64Array::from(vec![
            Some(1.0),
            None,
            Some(f64::NAN),
        ]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT c1 IS NOT NULL FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["true"], vec!["false"], vec!["true"]];

    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_count_distinct() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Int32, true)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int32Array::from(vec![
            Some(0),
            Some(1),
            None,
            Some(3),
            Some(3),
        ]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT COUNT(DISTINCT c1) FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["3".to_string()]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_on_string_dictionary() -> Result<()> {
    // Test to ensure DataFusion can operate on dictionary types
    // Use StringDictionary (32 bit indexes = keys)
    let field_type =
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8));
    let schema = Arc::new(Schema::new(vec![Field::new("d1", field_type, true)]));

    let keys_builder = PrimitiveBuilder::<Int32Type>::new(10);
    let values_builder = StringBuilder::new(10);
    let mut builder = StringDictionaryBuilder::new(keys_builder, values_builder);

    builder.append("one")?;
    builder.append_null()?;
    builder.append("three")?;
    let array = Arc::new(builder.finish());

    let data = RecordBatch::try_new(schema.clone(), vec![array])?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;
    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));

    // Basic SELECT
    let sql = "SELECT * FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["one"], vec!["NULL"], vec!["three"]];
    assert_eq!(expected, actual);

    // basic filtering
    let sql = "SELECT * FROM test WHERE d1 IS NOT NULL";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["one"], vec!["three"]];
    assert_eq!(expected, actual);

    // filtering with constant
    let sql = "SELECT * FROM test WHERE d1 = 'three'";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["three"]];
    assert_eq!(expected, actual);

    // Expression evaluation
    let sql = "SELECT concat(d1, '-foo') FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["one-foo"], vec!["-foo"], vec!["three-foo"]];
    assert_eq!(expected, actual);

    // aggregation
    let sql = "SELECT COUNT(d1) FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["2"]];
    assert_eq!(expected, actual);

    Ok(())
}

#[tokio::test]
async fn query_without_from() -> Result<()> {
    // Test for SELECT <expression> without FROM.
    // Should evaluate expressions in project position.
    let mut ctx = ExecutionContext::new();

    let sql = "SELECT 1";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["1"]];
    assert_eq!(expected, actual);

    let sql = "SELECT 1+2, 3/4, cos(0)";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["3", "0", "1"]];
    assert_eq!(expected, actual);

    Ok(())
}

#[tokio::test]
async fn query_scalar_minus_array() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Int32, true)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int32Array::from(vec![
            Some(0),
            Some(1),
            None,
            Some(3),
        ]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = "SELECT 4 - c1 FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["4"], vec!["3"], vec!["NULL"], vec!["1"]];
    assert_eq!(expected, actual);
    Ok(())
}

fn assert_float_eq<T>(expected: &[Vec<T>], received: &[Vec<String>])
where
    T: AsRef<str>,
{
    expected
        .iter()
        .flatten()
        .zip(received.iter().flatten())
        .for_each(|(l, r)| {
            let (l, r) = (
                l.as_ref().parse::<f64>().unwrap(),
                r.as_str().parse::<f64>().unwrap(),
            );
            assert!((l - r).abs() <= 2.0 * f64::EPSILON);
        });
}

#[tokio::test]
async fn csv_between_expr() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c4 FROM aggregate_test_100 WHERE c12 BETWEEN 0.995 AND 1.0";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![vec!["10837"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_between_expr_negated() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv(&mut ctx)?;
    let sql = "SELECT c4 FROM aggregate_test_100 WHERE c12 NOT BETWEEN 0 AND 0.995";
    let mut actual = execute(&mut ctx, sql).await;
    actual.sort();
    let expected = vec![vec!["10837"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn csv_group_by_date() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    let schema = Arc::new(Schema::new(vec![
        Field::new("date", DataType::Date32, false),
        Field::new("cnt", DataType::Int32, false),
    ]));
    let data = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Date32Array::from(vec![
                Some(100),
                Some(100),
                Some(100),
                Some(101),
                Some(101),
                Some(101),
            ])),
            Arc::new(Int32Array::from(vec![
                Some(1),
                Some(2),
                Some(3),
                Some(3),
                Some(3),
                Some(3),
            ])),
        ],
    )?;
    let table = MemTable::try_new(schema, vec![vec![data]])?;

    ctx.register_table("dates", Arc::new(table));
    let sql = "SELECT SUM(cnt) FROM dates GROUP BY date";
    let actual = execute(&mut ctx, sql).await;
    let mut actual: Vec<String> = actual.iter().flatten().cloned().collect();
    actual.sort();
    let expected = vec!["6", "9"];
    assert_eq!(expected, actual);
    Ok(())
}

macro_rules! test_expression {
    ($SQL:expr, $EXPECTED:expr) => {
        let mut ctx = ExecutionContext::new();
        let sql = format!("SELECT {}", $SQL);
        let actual = execute(&mut ctx, sql.as_str()).await;
        assert_eq!($EXPECTED, actual[0][0]);
    };
}

#[tokio::test]
async fn test_string_expressions() -> Result<()> {
    test_expression!("ascii('')", "0");
    test_expression!("ascii('x')", "120");
    test_expression!("ascii(NULL)", "NULL");
    test_expression!("bit_length('')", "0");
    test_expression!("bit_length('chars')", "40");
    test_expression!("bit_length('josé')", "40");
    test_expression!("bit_length(NULL)", "NULL");
    test_expression!("btrim(' xyxtrimyyx ', NULL)", "NULL");
    test_expression!("btrim(' xyxtrimyyx ')", "xyxtrimyyx");
    test_expression!("btrim('\n xyxtrimyyx \n')", "\n xyxtrimyyx \n");
    test_expression!("btrim('xyxtrimyyx', 'xyz')", "trim");
    test_expression!("btrim('\nxyxtrimyyx\n', 'xyz\n')", "trim");
    test_expression!("btrim(NULL, 'xyz')", "NULL");
    test_expression!("char_length('')", "0");
    test_expression!("char_length('chars')", "5");
    test_expression!("char_length(NULL)", "NULL");
    test_expression!("character_length('')", "0");
    test_expression!("character_length('chars')", "5");
    test_expression!("character_length('josé')", "4");
    test_expression!("character_length(NULL)", "NULL");
    test_expression!("chr(CAST(120 AS int))", "x");
    test_expression!("chr(CAST(128175 AS int))", "💯");
    test_expression!("chr(CAST(NULL AS int))", "NULL");
    test_expression!("concat('a','b','c')", "abc");
    test_expression!("concat('abcde', 2, NULL, 22)", "abcde222");
    test_expression!("concat(NULL)", "");
    test_expression!("concat_ws(',', 'abcde', 2, NULL, 22)", "abcde,2,22");
    test_expression!("concat_ws('|','a','b','c')", "a|b|c");
    test_expression!("concat_ws('|',NULL)", "");
    test_expression!("concat_ws(NULL,'a',NULL,'b','c')", "NULL");
    test_expression!("initcap('')", "");
    test_expression!("initcap('hi THOMAS')", "Hi Thomas");
    test_expression!("initcap(NULL)", "NULL");
    test_expression!("left('abcde', -2)", "abc");
    test_expression!("left('abcde', -200)", "");
    test_expression!("left('abcde', 0)", "");
    test_expression!("left('abcde', 2)", "ab");
    test_expression!("left('abcde', 200)", "abcde");
    test_expression!("left('abcde', CAST(NULL AS INT))", "NULL");
    test_expression!("left(NULL, 2)", "NULL");
    test_expression!("left(NULL, CAST(NULL AS INT))", "NULL");
    test_expression!("lower('')", "");
    test_expression!("lower('TOM')", "tom");
    test_expression!("lower(NULL)", "NULL");
    test_expression!("lpad('hi', 5, 'xy')", "xyxhi");
    test_expression!("lpad('hi', 0)", "");
    test_expression!("lpad('hi', 21, 'abcdef')", "abcdefabcdefabcdefahi");
    test_expression!("lpad('hi', 5, 'xy')", "xyxhi");
    test_expression!("lpad('hi', 5, NULL)", "NULL");
    test_expression!("lpad('hi', 5)", "   hi");
    test_expression!("lpad('hi', CAST(NULL AS INT), 'xy')", "NULL");
    test_expression!("lpad('hi', CAST(NULL AS INT))", "NULL");
    test_expression!("lpad('xyxhi', 3)", "xyx");
    test_expression!("lpad(NULL, 0)", "NULL");
    test_expression!("lpad(NULL, 5, 'xy')", "NULL");
    test_expression!("ltrim(' zzzytest ', NULL)", "NULL");
    test_expression!("ltrim(' zzzytest ')", "zzzytest ");
    test_expression!("ltrim('zzzytest', 'xyz')", "test");
    test_expression!("ltrim(NULL, 'xyz')", "NULL");
    test_expression!("octet_length('')", "0");
    test_expression!("octet_length('chars')", "5");
    test_expression!("octet_length('josé')", "5");
    test_expression!("octet_length(NULL)", "NULL");
    test_expression!("repeat('Pg', 4)", "PgPgPgPg");
    test_expression!("repeat('Pg', CAST(NULL AS INT))", "NULL");
    test_expression!("repeat(NULL, 4)", "NULL");
    test_expression!("reverse('abcde')", "edcba");
    test_expression!("reverse('loẅks')", "skẅol");
    test_expression!("reverse(NULL)", "NULL");
    test_expression!("right('abcde', -2)", "cde");
    test_expression!("right('abcde', -200)", "");
    test_expression!("right('abcde', 0)", "");
    test_expression!("right('abcde', 2)", "de");
    test_expression!("right('abcde', 200)", "abcde");
    test_expression!("right('abcde', CAST(NULL AS INT))", "NULL");
    test_expression!("right(NULL, 2)", "NULL");
    test_expression!("right(NULL, CAST(NULL AS INT))", "NULL");
    test_expression!("rpad('hi', 5, 'xy')", "hixyx");
    test_expression!("rpad('hi', 0)", "");
    test_expression!("rpad('hi', 21, 'abcdef')", "hiabcdefabcdefabcdefa");
    test_expression!("rpad('hi', 5, 'xy')", "hixyx");
    test_expression!("rpad('hi', 5, NULL)", "NULL");
    test_expression!("rpad('hi', 5)", "hi   ");
    test_expression!("rpad('hi', CAST(NULL AS INT), 'xy')", "NULL");
    test_expression!("rpad('hi', CAST(NULL AS INT))", "NULL");
    test_expression!("rpad('xyxhi', 3)", "xyx");
    test_expression!("rtrim(' testxxzx ')", " testxxzx");
    test_expression!("rtrim(' zzzytest ', NULL)", "NULL");
    test_expression!("rtrim('testxxzx', 'xyz')", "test");
    test_expression!("rtrim(NULL, 'xyz')", "NULL");
    test_expression!("substr('alphabet', -3)", "alphabet");
    test_expression!("substr('alphabet', 0)", "alphabet");
    test_expression!("substr('alphabet', 1)", "alphabet");
    test_expression!("substr('alphabet', 2)", "lphabet");
    test_expression!("substr('alphabet', 3)", "phabet");
    test_expression!("substr('alphabet', 30)", "");
    test_expression!("substr('alphabet', CAST(NULL AS int))", "NULL");
    test_expression!("substr('alphabet', 3, 2)", "ph");
    test_expression!("substr('alphabet', 3, 20)", "phabet");
    test_expression!("substr('alphabet', CAST(NULL AS int), 20)", "NULL");
    test_expression!("substr('alphabet', 3, CAST(NULL AS int))", "NULL");
    test_expression!("to_hex(2147483647)", "7fffffff");
    test_expression!("to_hex(9223372036854775807)", "7fffffffffffffff");
    test_expression!("to_hex(CAST(NULL AS int))", "NULL");
    test_expression!("trim(' tom ')", "tom");
    test_expression!("trim(' tom')", "tom");
    test_expression!("trim('')", "");
    test_expression!("trim('tom ')", "tom");
    test_expression!("upper('')", "");
    test_expression!("upper('tom')", "TOM");
    test_expression!("upper(NULL)", "NULL");
    Ok(())
}

#[tokio::test]
async fn test_boolean_expressions() -> Result<()> {
    test_expression!("true", "true");
    test_expression!("false", "false");
    Ok(())
}

#[tokio::test]
async fn test_interval_expressions() -> Result<()> {
    test_expression!(
        "interval '1'",
        "0 years 0 mons 0 days 0 hours 0 mins 1.00 secs"
    );
    test_expression!(
        "interval '1 second'",
        "0 years 0 mons 0 days 0 hours 0 mins 1.00 secs"
    );
    test_expression!(
        "interval '500 milliseconds'",
        "0 years 0 mons 0 days 0 hours 0 mins 0.500 secs"
    );
    test_expression!(
        "interval '5 second'",
        "0 years 0 mons 0 days 0 hours 0 mins 5.00 secs"
    );
    test_expression!(
        "interval '0.5 minute'",
        "0 years 0 mons 0 days 0 hours 0 mins 30.00 secs"
    );
    test_expression!(
        "interval '.5 minute'",
        "0 years 0 mons 0 days 0 hours 0 mins 30.00 secs"
    );
    test_expression!(
        "interval '5 minute'",
        "0 years 0 mons 0 days 0 hours 5 mins 0.00 secs"
    );
    test_expression!(
        "interval '5 minute 1 second'",
        "0 years 0 mons 0 days 0 hours 5 mins 1.00 secs"
    );
    test_expression!(
        "interval '1 hour'",
        "0 years 0 mons 0 days 1 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '5 hour'",
        "0 years 0 mons 0 days 5 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '1 day'",
        "0 years 0 mons 1 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '1 day 1'",
        "0 years 0 mons 1 days 0 hours 0 mins 1.00 secs"
    );
    test_expression!(
        "interval '0.5'",
        "0 years 0 mons 0 days 0 hours 0 mins 0.500 secs"
    );
    test_expression!(
        "interval '0.5 day 1'",
        "0 years 0 mons 0 days 12 hours 0 mins 1.00 secs"
    );
    test_expression!(
        "interval '0.49 day'",
        "0 years 0 mons 0 days 11 hours 45 mins 36.00 secs"
    );
    test_expression!(
        "interval '0.499 day'",
        "0 years 0 mons 0 days 11 hours 58 mins 33.596 secs"
    );
    test_expression!(
        "interval '0.4999 day'",
        "0 years 0 mons 0 days 11 hours 59 mins 51.364 secs"
    );
    test_expression!(
        "interval '0.49999 day'",
        "0 years 0 mons 0 days 11 hours 59 mins 59.136 secs"
    );
    test_expression!(
        "interval '0.49999999999 day'",
        "0 years 0 mons 0 days 12 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '5 day'",
        "0 years 0 mons 5 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '5 day 4 hours 3 minutes 2 seconds 100 milliseconds'",
        "0 years 0 mons 5 days 4 hours 3 mins 2.100 secs"
    );
    test_expression!(
        "interval '0.5 month'",
        "0 years 0 mons 15 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '1 month'",
        "0 years 1 mons 0 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '5 month'",
        "0 years 5 mons 0 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '13 month'",
        "1 years 1 mons 0 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '0.5 year'",
        "0 years 6 mons 0 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '1 year'",
        "1 years 0 mons 0 days 0 hours 0 mins 0.00 secs"
    );
    test_expression!(
        "interval '2 year'",
        "2 years 0 mons 0 days 0 hours 0 mins 0.00 secs"
    );
    Ok(())
}

#[tokio::test]
#[cfg_attr(not(feature = "crypto_expressions"), ignore)]
async fn test_crypto_expressions() -> Result<()> {
    test_expression!("md5('tom')", "34b7da764b21d298ef307d04d8152dc5");
    test_expression!("md5('')", "d41d8cd98f00b204e9800998ecf8427e");
    test_expression!("md5(NULL)", "NULL");
    test_expression!(
        "sha224('tom')",
        "0bf6cb62649c42a9ae3876ab6f6d92ad36cb5414e495f8873292be4d"
    );
    test_expression!(
        "sha224('')",
        "d14a028c2a3a2bc9476102bb288234c415a2b01f828ea62ac5b3e42f"
    );
    test_expression!("sha224(NULL)", "NULL");
    test_expression!(
        "sha256('tom')",
        "e1608f75c5d7813f3d4031cb30bfb786507d98137538ff8e128a6ff74e84e643"
    );
    test_expression!(
        "sha256('')",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    test_expression!("sha256(NULL)", "NULL");
    test_expression!("sha384('tom')", "096f5b68aa77848e4fdf5c1c0b350de2dbfad60ffd7c25d9ea07c6c19b8a4d55a9187eb117c557883f58c16dfac3e343");
    test_expression!("sha384('')", "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b");
    test_expression!("sha384(NULL)", "NULL");
    test_expression!("sha512('tom')", "6e1b9b3fe840680e37051f7ad5e959d6f39ad0f8885d855166f55c659469d3c8b78118c44a2a49c72ddb481cd6d8731034e11cc030070ba843a90b3495cb8d3e");
    test_expression!("sha512('')", "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e");
    test_expression!("sha512(NULL)", "NULL");
    Ok(())
}

#[tokio::test]
async fn test_extract_date_part() -> Result<()> {
    test_expression!("date_part('hour', CAST('2020-01-01' AS DATE))", "0");
    test_expression!("EXTRACT(HOUR FROM CAST('2020-01-01' AS DATE))", "0");
    test_expression!(
        "EXTRACT(HOUR FROM to_timestamp('2020-09-08T12:00:00+00:00'))",
        "12"
    );
    test_expression!("date_part('YEAR', CAST('2000-01-01' AS DATE))", "2000");
    test_expression!(
        "EXTRACT(year FROM to_timestamp('2020-09-08T12:00:00+00:00'))",
        "2020"
    );
    Ok(())
}

#[tokio::test]
async fn test_in_list_scalar() -> Result<()> {
    test_expression!("'a' IN ('a','b')", "true");
    test_expression!("'c' IN ('a','b')", "false");
    test_expression!("'c' NOT IN ('a','b')", "true");
    test_expression!("'a' NOT IN ('a','b')", "false");
    test_expression!("NULL IN ('a','b')", "NULL");
    test_expression!("NULL NOT IN ('a','b')", "NULL");
    test_expression!("'a' IN ('a','b',NULL)", "true");
    test_expression!("'c' IN ('a','b',NULL)", "NULL");
    test_expression!("'a' NOT IN ('a','b',NULL)", "false");
    test_expression!("'c' NOT IN ('a','b',NULL)", "NULL");
    test_expression!("0 IN (0,1,2)", "true");
    test_expression!("3 IN (0,1,2)", "false");
    test_expression!("3 NOT IN (0,1,2)", "true");
    test_expression!("0 NOT IN (0,1,2)", "false");
    test_expression!("NULL IN (0,1,2)", "NULL");
    test_expression!("NULL NOT IN (0,1,2)", "NULL");
    test_expression!("0 IN (0,1,2,NULL)", "true");
    test_expression!("3 IN (0,1,2,NULL)", "NULL");
    test_expression!("0 NOT IN (0,1,2,NULL)", "false");
    test_expression!("3 NOT IN (0,1,2,NULL)", "NULL");
    test_expression!("0.0 IN (0.0,0.1,0.2)", "true");
    test_expression!("0.3 IN (0.0,0.1,0.2)", "false");
    test_expression!("0.3 NOT IN (0.0,0.1,0.2)", "true");
    test_expression!("0.0 NOT IN (0.0,0.1,0.2)", "false");
    test_expression!("NULL IN (0.0,0.1,0.2)", "NULL");
    test_expression!("NULL NOT IN (0.0,0.1,0.2)", "NULL");
    test_expression!("0.0 IN (0.0,0.1,0.2,NULL)", "true");
    test_expression!("0.3 IN (0.0,0.1,0.2,NULL)", "NULL");
    test_expression!("0.0 NOT IN (0.0,0.1,0.2,NULL)", "false");
    test_expression!("0.3 NOT IN (0.0,0.1,0.2,NULL)", "NULL");
    test_expression!("'1' IN ('a','b',1)", "true");
    test_expression!("'2' IN ('a','b',1)", "false");
    test_expression!("'2' NOT IN ('a','b',1)", "true");
    test_expression!("'1' NOT IN ('a','b',1)", "false");
    test_expression!("NULL IN ('a','b',1)", "NULL");
    test_expression!("NULL NOT IN ('a','b',1)", "NULL");
    test_expression!("'1' IN ('a','b',NULL,1)", "true");
    test_expression!("'2' IN ('a','b',NULL,1)", "NULL");
    test_expression!("'1' NOT IN ('a','b',NULL,1)", "false");
    test_expression!("'2' NOT IN ('a','b',NULL,1)", "NULL");
    Ok(())
}

#[tokio::test]
async fn in_list_array() -> Result<()> {
    let mut ctx = ExecutionContext::new();
    register_aggregate_csv_by_sql(&mut ctx).await;
    let sql = "SELECT
            c1 IN ('a', 'c') AS utf8_in_true
            ,c1 IN ('x', 'y') AS utf8_in_false
            ,c1 NOT IN ('x', 'y') AS utf8_not_in_true
            ,c1 NOT IN ('a', 'c') AS utf8_not_in_false
            ,CAST(CAST(c1 AS int) AS varchar) IN ('a', 'c') AS utf8_in_null
        FROM aggregate_test_100 WHERE c12 < 0.05";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![
        vec!["true", "false", "true", "false", "NULL"],
        vec!["true", "false", "true", "false", "NULL"],
        vec!["true", "false", "true", "false", "NULL"],
        vec!["false", "false", "true", "true", "NULL"],
        vec!["false", "false", "true", "true", "NULL"],
        vec!["false", "false", "true", "true", "NULL"],
        vec!["false", "false", "true", "true", "NULL"],
    ];
    assert_eq!(expected, actual);
    Ok(())
}

// TODO Tests to prove correct implementation of INNER JOIN's with qualified names.
//  https://issues.apache.org/jira/projects/ARROW/issues/ARROW-11432.
#[tokio::test]
#[ignore]
async fn inner_join_qualified_names() -> Result<()> {
    // Setup the statements that test qualified names function correctly.
    let equivalent_sql = [
        "SELECT t1.a, t1.b, t1.c, t2.a, t2.b, t2.c
            FROM t1
            INNER JOIN t2 ON t1.a = t2.a
            ORDER BY t1.a",
        "SELECT t1.a, t1.b, t1.c, t2.a, t2.b, t2.c
            FROM t1
            INNER JOIN t2 ON t2.a = t1.a
            ORDER BY t1.a",
    ];

    let expected = vec![
        vec!["1", "10", "50", "1", "100", "500"],
        vec!["2", "20", "60", "2", "20", "600"],
        vec!["4", "40", "80", "4", "400", "800"],
    ];

    for sql in equivalent_sql.iter() {
        let mut ctx = create_join_context_qualified()?;
        let actual = execute(&mut ctx, sql).await;
        assert_eq!(expected, actual);
    }
    Ok(())
}

#[tokio::test]
async fn query_regexp_extract() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Utf8, false)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["aaa-0", "bb-1", "aa"]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = r"SELECT regexp_extract(c1, '.*-(\d)', 1) FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["0"], vec!["1"], vec!["NULL"]];
    assert_eq!(expected, actual);
    Ok(())
}

#[tokio::test]
async fn query_regexp_match() -> Result<()> {
    let schema = Arc::new(Schema::new(vec![Field::new("c1", DataType::Utf8, false)]));

    let data = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec!["aaa-0", "bb-1", "aa"]))],
    )?;

    let table = MemTable::try_new(schema, vec![vec![data]])?;

    let mut ctx = ExecutionContext::new();
    ctx.register_table("test", Arc::new(table));
    let sql = r"SELECT regexp_match(c1, '.*-(\d)') FROM test";
    let actual = execute(&mut ctx, sql).await;
    let expected = vec![vec!["[0]"], vec!["[1]"], vec!["[]"]];
    assert_eq!(expected, actual);
    Ok(())
}
