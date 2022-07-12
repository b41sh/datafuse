//  Copyright 2021 Datafuse Labs.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
//

use std::borrow::Borrow;
use std::collections::HashMap;

use common_datavalues::DataValue;
use common_exception::Result;

use crate::storages::fuse::meta::BlockMeta;
use crate::storages::fuse::meta::ColumnId;
use crate::storages::fuse::meta::Statistics;
use crate::storages::index::ColumnStatistics;
use crate::storages::index::StatisticsOfColumns;
use crate::storages::index::StatisticsOfSubColumns;

pub fn reduce_block_statistics<T: Borrow<StatisticsOfColumns>>(
    stats: &[T],
) -> Result<StatisticsOfColumns> {
    // Combine statistics of a column into `Vec`, that is:
    // from : `&[HashMap<ColumnId, ColumnStatistics>]`
    // to   : `HashMap<ColumnId, Vec<&ColumnStatistics>)>`
    let col_stat_list = stats.iter().fold(HashMap::new(), |acc, item| {
        item.borrow().iter().fold(
            acc,
            |mut acc: HashMap<ColumnId, Vec<&ColumnStatistics>>, (col_id, stats)| {
                acc.entry(*col_id)
                    .or_insert_with(|| vec![stats])
                    .push(stats);
                acc
            },
        )
    });

    // Reduce the `Vec<&ColumnStatistics` into ColumnStatistics`, i.e.:
    // from : `HashMap<ColumnId, Vec<&ColumnStatistics>)>`
    // to   : `type BlockStatistics = HashMap<ColumnId, ColumnStatistics>`
    let len = stats.len();
    col_stat_list
        .iter()
        .try_fold(HashMap::with_capacity(len), |mut acc, (id, stats)| {
            let mut min_stats = Vec::with_capacity(stats.len());
            let mut max_stats = Vec::with_capacity(stats.len());
            let mut null_count = 0;
            let mut in_memory_size = 0;

            for col_stats in stats {
                min_stats.push(col_stats.min.clone());
                max_stats.push(col_stats.max.clone());

                null_count += col_stats.null_count;
                in_memory_size += col_stats.in_memory_size;
            }

            // TODO:

            // for some data types, we shall balance the accuracy and the length
            // e.g. for a string col, which max value is "abcdef....", we record the max as something like "b"

            // In accumulator.rs, we use aggregation functions to get the min/max of `DataValue`s,
            // like this:
            //   `let maxs = eval_aggr("max", vec![], &[column_field], rows)?`
            // we should unify these logics, or at least, ensure the ways they compares do NOT diverge
            let min = min_stats
                .iter()
                .filter(|s| !s.is_null())
                .min_by(|&x, &y| x.cmp(y))
                .cloned()
                .unwrap_or(DataValue::Null);

            let max = max_stats
                .iter()
                .filter(|s| !s.is_null())
                .max_by(|&x, &y| x.cmp(y))
                .cloned()
                .unwrap_or(DataValue::Null);

            acc.insert(*id, ColumnStatistics {
                min,
                max,
                null_count,
                in_memory_size,
            });
            Ok(acc)
        })
}

pub fn reduce_block_sub_statistics<T: Borrow<StatisticsOfSubColumns>>(
    sub_stats: &[T],
) -> Result<StatisticsOfSubColumns> {
    // Combine statistics of a column into `Vec`, that is:
    // from : `&[HashMap<String, ColumnStatistics>]`
    // to   : `HashMap<String, Vec<&ColumnStatistics>)>`
    let col_stat_list = sub_stats.iter().fold(HashMap::new(), |acc, item| {
        item.borrow().iter().fold(
            acc,
            |mut acc: HashMap<String, Vec<&ColumnStatistics>>, (col_key, sub_stats)| {
                acc.entry(col_key.clone())
                    .or_insert_with(|| vec![sub_stats])
                    .push(sub_stats);
                acc
            },
        )
    });

    // Reduce the `Vec<&ColumnStatistics` into ColumnStatistics`, i.e.:
    // from : `HashMap<String, Vec<&ColumnStatistics>)>`
    // to   : `type BlockStatistics = HashMap<String, ColumnStatistics>`
    let len = sub_stats.len();
    col_stat_list.iter().try_fold(
        HashMap::with_capacity(len),
        |mut acc, (col_key, sub_stats)| {
            let mut min_stats = Vec::with_capacity(sub_stats.len());
            let mut max_stats = Vec::with_capacity(sub_stats.len());
            let mut unset_bits = 0;
            let mut in_memory_size = 0;

            for col_stats in sub_stats {
                min_stats.push(col_stats.min.clone());
                max_stats.push(col_stats.max.clone());

                unset_bits += col_stats.unset_bits;
                in_memory_size += col_stats.in_memory_size;
            }

            // TODO:

            // for some data types, we shall balance the accuracy and the length
            // e.g. for a string col, which max value is "abcdef....", we record the max as something like "b"

            // In accumulator.rs, we use aggregation functions to get the min/max of `DataValue`s,
            // like this:
            //   `let maxs = eval_aggr("max", vec![], &[column_field], rows)?`
            // we should unify these logics, or at least, ensure the ways they compares do NOT diverge
            let min = min_stats
                .iter()
                .filter(|s| !s.is_null())
                .min_by(|&x, &y| x.cmp(y))
                .cloned()
                .unwrap_or(DataValue::Null);

            let max = max_stats
                .iter()
                .filter(|s| !s.is_null())
                .max_by(|&x, &y| x.cmp(y))
                .cloned()
                .unwrap_or(DataValue::Null);

            acc.insert(col_key.clone(), ColumnStatistics {
                min,
                max,
                unset_bits,
                in_memory_size,
            });
            Ok(acc)
        },
    )
}

pub fn merge_statistics(l: &Statistics, r: &Statistics) -> Result<Statistics> {
    let sub_col_stats = match (l.sub_col_stats.clone(), r.sub_col_stats.clone()) {
        (None, None) => None,
        (Some(l_sub_col_stats), None) => Some(l_sub_col_stats.clone()),
        (None, Some(r_sub_col_stats)) => Some(r_sub_col_stats.clone()),
        (Some(l_sub_col_stats), Some(r_sub_col_stats)) => Some(reduce_block_sub_statistics(&[
            &l_sub_col_stats,
            &r_sub_col_stats,
        ])?),
    };

    let s = Statistics {
        row_count: l.row_count + r.row_count,
        block_count: l.block_count + r.block_count,
        uncompressed_byte_size: l.uncompressed_byte_size + r.uncompressed_byte_size,
        compressed_byte_size: l.compressed_byte_size + r.compressed_byte_size,
        col_stats: reduce_block_statistics(&[&l.col_stats, &r.col_stats])?,
        sub_col_stats: sub_col_stats,
    };
    Ok(s)
}

pub fn reduce_statistics<T: Borrow<Statistics>>(stats: &[T]) -> Result<Statistics> {
    let mut statistics = Statistics::default();
    for item in stats {
        statistics = merge_statistics(&statistics, item.borrow())?
    }
    Ok(statistics)
}

pub fn reduce_block_metas<T: Borrow<BlockMeta>>(block_metas: &[T]) -> Result<Statistics> {
    let mut row_count: u64 = 0;
    let mut block_count: u64 = 0;
    let mut uncompressed_byte_size: u64 = 0;
    let mut compressed_byte_size: u64 = 0;

    block_metas.iter().for_each(|b| {
        let b = b.borrow();
        row_count += b.row_count;
        block_count += 1;
        uncompressed_byte_size += b.block_size;
        compressed_byte_size += b.file_size;
    });

    let stats = block_metas
        .iter()
        .map(|v| &v.borrow().col_stats)
        .collect::<Vec<_>>();
    let merged_col_stats = reduce_block_statistics(&stats)?;

    let sub_stats = block_metas
        .iter()
        .map(|v| v.borrow().sub_col_stats.clone())
        .filter(|s| s.is_some())
        .map(|s| s.unwrap())
        .collect::<Vec<_>>();
    let merged_sub_col_stats = if sub_stats.is_empty() {
        None
    } else {
        Some(reduce_block_sub_statistics(&sub_stats)?)
    };

    Ok(Statistics {
        row_count,
        block_count,
        uncompressed_byte_size,
        compressed_byte_size,
        col_stats: merged_col_stats,
        sub_col_stats: merged_sub_col_stats,
    })
}
