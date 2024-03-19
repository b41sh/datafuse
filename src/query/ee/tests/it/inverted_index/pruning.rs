// Copyright 2023 Databend Cloud
//
// Licensed under the Elastic License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.elastic.co/licensing/elastic-license
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use databend_common_ast::ast::Engine;
use databend_common_base::base::tokio;
use databend_common_catalog::plan::InvertedIndexInfo;
use databend_common_catalog::plan::PushDownInfo;
use databend_common_exception::Result;
use databend_common_expression::types::number::UInt64Type;
use databend_common_expression::types::NumberDataType;
use databend_common_expression::types::StringType;
use databend_common_expression::DataBlock;
use databend_common_expression::DataSchema;
use databend_common_expression::FromData;
use databend_common_expression::TableDataType;
use databend_common_expression::TableField;
use databend_common_expression::TableSchemaRef;
use databend_common_expression::TableSchemaRefExt;
use databend_common_meta_app::schema::CreateOption;
use databend_common_meta_app::schema::CreateTableIndexReq;
use databend_common_sql::plans::CreateTablePlan;
use databend_common_sql::BloomIndexColumns;
use databend_common_storages_fuse::pruning::create_segment_location_vector;
use databend_common_storages_fuse::pruning::FusePruner;
use databend_common_storages_fuse::FuseTable;
use databend_enterprise_inverted_index::get_inverted_index_handler;
use databend_enterprise_query::test_kits::context::EESetup;
use databend_query::interpreters::CreateTableInterpreter;
use databend_query::interpreters::Interpreter;
use databend_query::sessions::QueryContext;
use databend_query::sessions::TableContext;
use databend_query::storages::fuse::io::MetaReaders;
use databend_query::storages::fuse::FUSE_OPT_KEY_BLOCK_PER_SEGMENT;
use databend_query::storages::fuse::FUSE_OPT_KEY_ROW_PER_BLOCK;
use databend_query::test_kits::*;
use databend_storages_common_cache::LoadParams;
use databend_storages_common_pruner::BlockMetaIndex;
use databend_storages_common_table_meta::meta::BlockMeta;
use databend_storages_common_table_meta::meta::TableSnapshot;
use databend_storages_common_table_meta::meta::Versioned;
use databend_storages_common_table_meta::table::OPT_KEY_DATABASE_ID;
use opendal::Operator;

async fn apply_block_pruning(
    table_snapshot: Arc<TableSnapshot>,
    schema: TableSchemaRef,
    push_down: &Option<PushDownInfo>,
    ctx: Arc<QueryContext>,
    op: Operator,
    bloom_index_cols: BloomIndexColumns,
) -> Result<Vec<(BlockMetaIndex, Arc<BlockMeta>)>> {
    let index_info_locations = &table_snapshot.index_info_locations;
    let ctx: Arc<dyn TableContext> = ctx;
    let segment_locs = table_snapshot.segments.clone();
    let segment_locs = create_segment_location_vector(segment_locs, None);
    FusePruner::create(
        &ctx,
        op,
        schema,
        push_down,
        bloom_index_cols,
        index_info_locations,
    )
    .await?
    .read_pruning(segment_locs)
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn test_block_pruner() -> Result<()> {
    let fixture = TestFixture::setup_with_custom(EESetup::new()).await?;

    let ctx = fixture.new_query_ctx().await?;
    fixture.create_default_database().await?;

    let test_tbl_name = "test_index_helper";
    let test_schema = TableSchemaRefExt::create(vec![
        TableField::new("id", TableDataType::Number(NumberDataType::UInt64)),
        TableField::new("idiom", TableDataType::String),
        TableField::new("meaning", TableDataType::String),
    ]);

    let row_per_block = 5;
    let num_blocks_opt = row_per_block.to_string();

    // create test table
    let create_table_plan = CreateTablePlan {
        catalog: "default".to_owned(),
        create_option: CreateOption::Create,
        tenant: fixture.default_tenant(),
        database: fixture.default_db_name(),
        table: test_tbl_name.to_string(),
        schema: test_schema.clone(),
        engine: Engine::Fuse,
        engine_options: Default::default(),
        storage_params: None,
        read_only_attach: false,
        part_prefix: "".to_string(),
        options: [
            (FUSE_OPT_KEY_ROW_PER_BLOCK.to_owned(), num_blocks_opt),
            (FUSE_OPT_KEY_BLOCK_PER_SEGMENT.to_owned(), "5".to_owned()),
            (OPT_KEY_DATABASE_ID.to_owned(), "1".to_owned()),
        ]
        .into(),
        field_comments: vec![],
        as_select: None,
        cluster_key: None,
    };

    let interpreter = CreateTableInterpreter::try_create(ctx.clone(), create_table_plan)?;
    let _ = interpreter.execute(ctx.clone()).await?;

    // get table
    let catalog = ctx.get_catalog("default").await?;
    let table = catalog
        .get_table(
            fixture.default_tenant().as_str(),
            fixture.default_db_name().as_str(),
            test_tbl_name,
        )
        .await?;

    // prepare test blocks
    let block0 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![1, 2, 3, 4, 5]),
        StringType::from_data(vec![
            "A bird in the hand is worth two in the bush".to_string(),
            "A penny for your thoughts".to_string(),
            "A penny saved is a penny earned".to_string(),
            "A perfect storm".to_string(),
            "A picture is worth 1000 words".to_string(),
        ]),
        StringType::from_data(vec![
            "What you have is worth more than what you might have later".to_string(),
            "Tell me what you're thinking".to_string(),
            "Money you save today you can spend later".to_string(),
            "The worst possible situation".to_string(),
            "Better to show than tell".to_string(),
        ]),
    ]);
    let block1 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![6, 7, 8, 9, 10]),
        StringType::from_data(vec![
            "Actions speak louder than words".to_string(),
            "Add insult to injury".to_string(),
            "Barking up the wrong tree".to_string(),
            "Birds of a feather flock together".to_string(),
            "Bite off more than you can chew".to_string(),
        ]),
        StringType::from_data(vec![
            "Believe what people do and not what they say".to_string(),
            "To make a bad situation worse".to_string(),
            "To be mistaken, to be looking for solutions in the wrong place".to_string(),
            "People who are alike are often friends".to_string(),
            "Take on a project that you cannot finish".to_string(),
        ]),
    ]);
    let block2 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![11, 12, 13, 14, 15]),
        StringType::from_data(vec![
            "Break the ice".to_string(),
            "By the skin of your teeth".to_string(),
            "Comparing apples to oranges".to_string(),
            "Costs an arm and a leg".to_string(),
            "Do something at the drop of a hat".to_string(),
        ]),
        StringType::from_data(vec![
            "Make people feel more comfortable".to_string(),
            "Just barely".to_string(),
            "Comparing two things that cannot be compared".to_string(),
            "Very expensive".to_string(),
            "Do something without having planned beforehand".to_string(),
        ]),
    ]);
    let block3 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![16, 17, 18, 19, 20]),
        StringType::from_data(vec![
            "Do unto others as you would have them do unto you".to_string(),
            "Don't count your chickens before they hatch".to_string(),
            "Don't cry over spilt milk".to_string(),
            "Don't give up your day job".to_string(),
            "Don't put all your eggs in one basket".to_string(),
        ]),
        StringType::from_data(vec![
            "Treat people fairly. Also known as 'The Golden Rule'".to_string(),
            "Don't count on something good happening until it's happened".to_string(),
            "There's no reason to complain about something that can't be fixed".to_string(),
            "You're not very good at this".to_string(),
            "What you're doing is too risky".to_string(),
        ]),
    ]);
    let block4 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![21, 22, 23, 24, 25]),
        StringType::from_data(vec![
            "Every cloud has a silver lining".to_string(),
            "Get a taste of your own medicine".to_string(),
            "Give someone the cold shoulder".to_string(),
            "Go on a wild goose chase".to_string(),
            "Good things come to those who wait".to_string(),
        ]),
        StringType::from_data(vec![
            "Good things come after bad things".to_string(),
            "Get treated the way you've been treating others".to_string(),
            "Ignore someone".to_string(),
            "To do something pointless".to_string(),
            "Be patient".to_string(),
        ]),
    ]);
    let block5 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![26, 27, 28, 29, 30]),
        StringType::from_data(vec![
            "He has bigger fish to fry".to_string(),
            "He's a chip off the old block".to_string(),
            "Hit the nail on the head".to_string(),
            "Ignorance is bliss".to_string(),
            "It ain't over till the fat lady sings".to_string(),
        ]),
        StringType::from_data(vec![
            "He has bigger things to take care of than what we are talking about now".to_string(),
            "The son is like the father".to_string(),
            "Get something exactly right".to_string(),
            "You're better off not knowing".to_string(),
            "This isn't over yet".to_string(),
        ]),
    ]);
    let block6 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![31, 32, 33, 34, 35]),
        StringType::from_data(vec![
            "It takes one to know one".to_string(),
            "It's a piece of cake".to_string(),
            "It's raining cats and dogs".to_string(),
            "Kill two birds with one stone".to_string(),
            "Let the cat out of the bag".to_string(),
        ]),
        StringType::from_data(vec![
            "You're just as bad as I am".to_string(),
            "It's easy".to_string(),
            "It's raining hard".to_string(),
            "Get two things done with a single action".to_string(),
            "Give away a secret".to_string(),
        ]),
    ]);
    let block7 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![36, 37, 38, 39, 40]),
        StringType::from_data(vec![
            "Live and learn".to_string(),
            "Look before you leap".to_string(),
            "On thin ice".to_string(),
            "Once in a blue moon".to_string(),
            "Play devil's advocate".to_string(),
        ]),
        StringType::from_data(vec![
            "I made a mistake".to_string(),
            "Take only calculated risks".to_string(),
            "On probation. If you make another mistake, there will be trouble".to_string(),
            "Rarely".to_string(),
            "To argue the opposite, just for the sake of argument".to_string(),
        ]),
    ]);
    let block8 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![41, 42, 43, 44, 45]),
        StringType::from_data(vec![
            "Put something on ice".to_string(),
            "Rain on someone's parade".to_string(),
            "Saving for a rainy day".to_string(),
            "Slow and steady wins the race".to_string(),
            "Spill the beans".to_string(),
        ]),
        StringType::from_data(vec![
            "Put a projet on hold".to_string(),
            "To spoil something".to_string(),
            "Saving money for later".to_string(),
            "Reliability is more important than speed".to_string(),
            "Give away a secret".to_string(),
        ]),
    ]);
    let block9 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![46, 47, 48, 49, 50]),
        StringType::from_data(vec![
            "Take a rain check".to_string(),
            "Take it with a grain of salt".to_string(),
            "The ball is in your court".to_string(),
            "The best thing since sliced bread".to_string(),
            "The devil is in the details".to_string(),
        ]),
        StringType::from_data(vec![
            "Postpone a plan".to_string(),
            "Don’t take it too seriously".to_string(),
            "It's your decision".to_string(),
            "A really good invention".to_string(),
            "It looks good from a distance, but when you look closer, there are problems"
                .to_string(),
        ]),
    ]);
    let block10 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![51, 52, 53, 54, 55]),
        StringType::from_data(vec![
            "The early bird gets the worm".to_string(),
            "The elephant in the room".to_string(),
            "The whole nine yards".to_string(),
            "There are other fish in the sea".to_string(),
            "There's a method to his madness".to_string(),
        ]),
        StringType::from_data(vec![
            "The first people who arrive will get the best stuff".to_string(),
            "The big issue, the problem people are avoiding".to_string(),
            "Everything, all the way".to_string(),
            "It's ok to miss this opportunity. Others will arise".to_string(),
            "He seems crazy but actually he's clever".to_string(),
        ]),
    ]);
    let block11 = DataBlock::new_from_columns(vec![
        UInt64Type::from_data(vec![56, 57, 58, 59, 60]),
        StringType::from_data(vec![
            "There's no such thing as a free lunch".to_string(),
            "Throw caution to the wind".to_string(),
            "You can't have your cake and eat it too".to_string(),
            "You can't judge a book by its cover".to_string(),
            "A little learning is a dangerous thing".to_string(),
        ]),
        StringType::from_data(vec![
            "Nothing is entirely free".to_string(),
            "Take a risk".to_string(),
            "You can't have everything".to_string(),
            "This person or thing may look bad, but it's good inside".to_string(),
            "People who don't understand something fully are dangerous".to_string(),
        ]),
    ]);

    let blocks = vec![
        block0, block1, block2, block3, block4, block5, block6, block7, block8, block9, block10,
        block11,
    ];

    fixture
        .append_commit_blocks(table.clone(), blocks, false, true)
        .await?;

    let table = catalog
        .get_table(
            fixture.default_tenant().as_str(),
            fixture.default_db_name().as_str(),
            test_tbl_name,
        )
        .await?;

    // create inverted index on table
    let handler = get_inverted_index_handler();

    let table_ctx = fixture.new_query_ctx().await?;
    let catalog = table_ctx
        .get_catalog(&fixture.default_catalog_name())
        .await?;
    let table_id = table.get_id();
    let index_name = "idx1".to_string();
    let req = CreateTableIndexReq {
        create_option: CreateOption::Create,
        table_id,
        name: index_name.clone(),
        column_ids: vec![1, 2],
    };

    let res = handler.do_create_table_index(catalog, req).await;
    assert!(res.is_ok());

    let index_table_schema = TableSchemaRefExt::create(vec![
        TableField::new("idiom", TableDataType::String),
        TableField::new("meaning", TableDataType::String),
    ]);
    let fuse_table = FuseTable::try_from_table(table.as_ref())?;

    let new_snapshot_location = handler
        .do_refresh_index(
            fuse_table,
            table_ctx.clone(),
            index_name.clone(),
            index_table_schema.clone(),
            None,
        )
        .await?;
    assert!(new_snapshot_location.is_some());
    let new_snapshot_location = new_snapshot_location.unwrap();

    let reader = MetaReaders::table_snapshot_reader(fuse_table.get_operator());

    let load_params = LoadParams {
        location: new_snapshot_location.clone(),
        len_hint: None,
        ver: TableSnapshot::VERSION,
        put_cache: false,
    };
    let snapshot = reader.read(&load_params).await?;

    let index_schema = DataSchema::from(index_table_schema);
    let e1 = PushDownInfo {
        inverted_index: Some(InvertedIndexInfo {
            index_name: index_name.clone(),
            index_schema: index_schema.clone(),
            query_columns: vec!["idiom".to_string()],
            query_text: "test".to_string(),
        }),
        ..Default::default()
    };
    let e2 = PushDownInfo {
        inverted_index: Some(InvertedIndexInfo {
            index_name: index_name.clone(),
            index_schema: index_schema.clone(),
            query_columns: vec!["idiom".to_string()],
            query_text: "save".to_string(),
        }),
        ..Default::default()
    };
    let e3 = PushDownInfo {
        inverted_index: Some(InvertedIndexInfo {
            index_name: index_name.clone(),
            index_schema: index_schema.clone(),
            query_columns: vec!["idiom".to_string()],
            query_text: "one".to_string(),
        }),
        ..Default::default()
    };
    let e4 = PushDownInfo {
        inverted_index: Some(InvertedIndexInfo {
            index_name: index_name.clone(),
            index_schema: index_schema.clone(),
            query_columns: vec!["idiom".to_string()],
            query_text: "the".to_string(),
        }),
        ..Default::default()
    };
    let e5 = PushDownInfo {
        inverted_index: Some(InvertedIndexInfo {
            index_name: index_name.clone(),
            index_schema: index_schema.clone(),
            query_columns: vec!["meaning".to_string()],
            query_text: "people".to_string(),
        }),
        ..Default::default()
    };
    let e6 = PushDownInfo {
        inverted_index: Some(InvertedIndexInfo {
            index_name: index_name.clone(),
            index_schema: index_schema.clone(),
            query_columns: vec!["meaning".to_string()],
            query_text: "bad".to_string(),
        }),
        ..Default::default()
    };
    let extras = vec![
        (Some(e1), 0, 0),
        (Some(e2), 2, 2),
        (Some(e3), 2, 3),
        (Some(e4), 10, 20),
        (Some(e5), 5, 7),
        (Some(e6), 4, 4),
    ];
    for (extra, expected_blocks, expected_rows) in extras {
        let block_metas = apply_block_pruning(
            snapshot.clone(),
            table.get_table_info().schema(),
            &extra,
            ctx.clone(),
            fuse_table.get_operator(),
            fuse_table.bloom_index_cols(),
        )
        .await?;

        let rows = block_metas
            .iter()
            .map(|(v, _)| match &v.matched_rows {
                Some(matched_rows) => matched_rows.len(),
                None => 0,
            })
            .sum::<usize>();

        assert_eq!(expected_rows, rows);
        assert_eq!(expected_blocks, block_metas.len());
    }

    Ok(())
}
