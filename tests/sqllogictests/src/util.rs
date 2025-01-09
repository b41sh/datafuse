// Copyright 2021 Datafuse Labs
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

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use bollard::Docker;
use clap::Parser;
use redis::Commands;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use testcontainers::core::IntoContainerPort;
use testcontainers::core::WaitFor;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers::GenericImage;
use testcontainers::ImageExt;
use testcontainers_modules::mysql::Mysql;
use testcontainers_modules::redis::Redis;
use testcontainers_modules::redis::REDIS_PORT;
use walkdir::DirEntry;
use walkdir::WalkDir;

use crate::arg::SqlLogicTestArgs;
use crate::error::DSqlLogicTestError;
use crate::error::Result;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub id: String,
    pub start_time: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HttpSessionConf {
    pub database: Option<String>,
    pub role: Option<String>,
    pub secondary_roles: Option<Vec<String>>,
    pub settings: Option<BTreeMap<String, String>>,
    pub txn_state: Option<String>,
    pub last_server_info: Option<ServerInfo>,
    #[serde(default)]
    pub last_query_ids: Vec<String>,
    pub internal: Option<String>,
}

pub fn parser_rows(rows: &Value) -> Result<Vec<Vec<String>>> {
    let mut parsed_rows = Vec::new();
    for row in rows.as_array().unwrap() {
        let mut parsed_row = Vec::new();
        for col in row.as_array().unwrap() {
            match col {
                Value::Null => {
                    parsed_row.push("NULL".to_string());
                }
                Value::String(cell) => {
                    // If the result is empty, we'll use `(empty)` to mark it explicitly to avoid confusion
                    if cell.is_empty() {
                        parsed_row.push("(empty)".to_string());
                    } else {
                        parsed_row.push(cell.to_string());
                    }
                }
                _ => unreachable!(),
            }
        }
        parsed_rows.push(parsed_row);
    }
    Ok(parsed_rows)
}

fn find_specific_dir(dir: &str, suit: PathBuf) -> Result<DirEntry> {
    for entry in WalkDir::new(suit)
        .min_depth(0)
        .max_depth(100)
        .sort_by(|a, b| a.file_name().cmp(b.file_name()))
        .into_iter()
    {
        let e = entry.as_ref().unwrap();
        if e.file_type().is_dir() && e.file_name().to_str().unwrap() == dir {
            return Ok(entry?);
        }
    }
    Err(DSqlLogicTestError::SelfError(
        "Didn't find specific dir".to_string(),
    ))
}

pub fn get_files(suit: PathBuf) -> Result<Vec<walkdir::Result<DirEntry>>> {
    let args = SqlLogicTestArgs::parse();
    let mut files = vec![];

    let dirs = match args.dir {
        Some(ref dir) => {
            // Find specific dir
            let dir_entry = find_specific_dir(dir, suit);
            match dir_entry {
                Ok(dir_entry) => Some(dir_entry.into_path()),
                // If didn't find specific dir, return empty vec
                Err(_) => None,
            }
        }
        None => Some(suit),
    };
    let target = match dirs {
        Some(dir) => dir,
        None => return Ok(vec![]),
    };
    for entry in WalkDir::new(target)
        .min_depth(0)
        .max_depth(100)
        .sort_by(|a, b| a.file_name().cmp(b.file_name()))
        .into_iter()
        .filter_entry(|e| {
            if let Some(skipped_dir) = &args.skipped_dir {
                let dirs = skipped_dir.split(',').collect::<Vec<&str>>();
                if dirs.contains(&e.file_name().to_str().unwrap()) {
                    return false;
                }
            }
            true
        })
        .filter(|e| !e.as_ref().unwrap().file_type().is_dir())
    {
        files.push(entry);
    }
    Ok(files)
}

static PREPARE_TPCH: std::sync::Once = std::sync::Once::new();
static PREPARE_TPCDS: std::sync::Once = std::sync::Once::new();
static PREPARE_STAGE: std::sync::Once = std::sync::Once::new();
static PREPARE_SPILL: std::sync::Once = std::sync::Once::new();
static PREPARE_WASM: std::sync::Once = std::sync::Once::new();

#[derive(Eq, Hash, PartialEq)]
pub enum LazyDir {
    Tpch,
    Tpcds,
    Stage,
    UdfNative,
    Spill,
    Dictionaries,
}

pub fn collect_lazy_dir(file_path: &Path, lazy_dirs: &mut HashSet<LazyDir>) -> Result<()> {
    let file_path = file_path.to_str().unwrap_or_default();
    if file_path.contains("tpch/") {
        if !lazy_dirs.contains(&LazyDir::Tpch) {
            lazy_dirs.insert(LazyDir::Tpch);
        }
    } else if file_path.contains("tpcds/") {
        if !lazy_dirs.contains(&LazyDir::Tpcds) {
            lazy_dirs.insert(LazyDir::Tpcds);
        }
    } else if file_path.contains("stage/") || file_path.contains("stage_parquet/") {
        if !lazy_dirs.contains(&LazyDir::Stage) {
            lazy_dirs.insert(LazyDir::Stage);
        }
    } else if file_path.contains("udf_native/") {
        if !lazy_dirs.contains(&LazyDir::UdfNative) {
            lazy_dirs.insert(LazyDir::UdfNative);
        }
    } else if file_path.contains("spill/") {
        if !lazy_dirs.contains(&LazyDir::Spill) {
            lazy_dirs.insert(LazyDir::Spill);
        }
    } else if file_path.contains("dictionaries/") && !lazy_dirs.contains(&LazyDir::Dictionaries) {
        lazy_dirs.insert(LazyDir::Dictionaries);
    }
    Ok(())
}

pub fn lazy_prepare_data(lazy_dirs: &HashSet<LazyDir>) -> Result<()> {
    for lazy_dir in lazy_dirs {
        match lazy_dir {
            LazyDir::Tpch => {
                PREPARE_TPCH.call_once(|| {
                    println!("Calling the script prepare_tpch_data.sh ...");
                    run_script("prepare_tpch_data.sh").unwrap();
                });
            }
            LazyDir::Tpcds => {
                PREPARE_TPCDS.call_once(|| {
                    println!("Calling the script prepare_tpcds_data.sh ...");
                    run_script("prepare_tpcds_data.sh").unwrap();
                });
            }
            LazyDir::Stage => {
                PREPARE_STAGE.call_once(|| {
                    println!("Calling the script prepare_stage.sh ...");
                    run_script("prepare_stage.sh").unwrap();
                });
            }
            LazyDir::UdfNative => {
                println!("wasm context Calling the script prepare_stage.sh ...");
                PREPARE_WASM.call_once(|| run_script("prepare_stage.sh").unwrap())
            }
            LazyDir::Spill => {
                println!("Calling the script prepare_spill_data.sh ...");
                PREPARE_SPILL.call_once(|| run_script("prepare_spill_data.sh").unwrap())
            }
            _ => {}
        }
    }
    Ok(())
}

fn run_script(name: &str) -> Result<()> {
    let path = format!("tests/sqllogictests/scripts/{}", name);
    let output = std::process::Command::new("bash")
        .arg(path)
        .output()
        .expect("failed to execute process");
    if !output.status.success() {
        return Err(DSqlLogicTestError::SelfError(format!(
            "Failed to run {}: {}",
            name,
            String::from_utf8(output.stderr).unwrap()
        )));
    }
    Ok(())
}

pub async fn run_ttc_container(
    docker: &Docker,
    image: &str,
    port: u16,
    cs: &mut Vec<ContainerAsync<GenericImage>>,
) -> Result<()> {
    let mut images = image.split(":");
    let image = images.next().unwrap();
    let tag = images.next().unwrap_or("latest");

    let container_name = format!("databend-ttc-{}", port);
    println!("Start {container_name}");

    // Stop the container
    let _ = docker.stop_container(&container_name, None).await;
    let _ = docker.remove_container(&container_name, None).await;

    let container_res = GenericImage::new(image, tag)
        .with_exposed_port(port.tcp())
        .with_wait_for(WaitFor::Duration {
            length: Duration::from_secs(300),
        })
        .with_network("host")
        .with_env_var(
            "DATABEND_DSN",
            "databend://root:@127.0.0.1:8000?sslmode=disable",
        )
        .with_env_var("TTC_PORT", format!("{port}"))
        .with_container_name(&container_name)
        .start()
        .await;
    match container_res {
        Ok(container) => {
            println!("Started container: {}", container.id());
            cs.push(container);
            Ok(())
        }
        Err(e) => Err(format!("Start {container_name} failed: {e}").into()),
    }
}

#[allow(dead_code)]
pub struct DictionaryContainer {
    pub redis: ContainerAsync<Redis>,
    pub mysql: ContainerAsync<Mysql>,
}

pub async fn lazy_run_dictionary_containers(
    lazy_dirs: &HashSet<LazyDir>,
) -> Result<Option<DictionaryContainer>> {
    if !lazy_dirs.contains(&LazyDir::Dictionaries) {
        return Ok(None);
    }
    let docker = Docker::connect_with_local_defaults().unwrap();
    println!("run dictionary source server container");
    let redis = run_redis_server(&docker).await?;
    let mysql = run_mysql_server(&docker).await?;
    let dict_container = DictionaryContainer { redis, mysql };

    Ok(Some(dict_container))
}

async fn run_redis_server(docker: &Docker) -> Result<ContainerAsync<Redis>> {
    let container_name = "redis".to_string();

    // Stop the container
    let _ = docker.stop_container(&container_name, None).await;
    let _ = docker.remove_container(&container_name, None).await;

    let redis_res = Redis::default()
        .with_network("host")
        .with_startup_timeout(Duration::from_secs(300))
        .with_container_name(&container_name)
        .start()
        .await;

    match redis_res {
        Ok(redis) => {
            let host_ip = redis.get_host().await.unwrap();
            let url = format!("redis://{}:{}", host_ip, REDIS_PORT);
            let client = redis::Client::open(url.as_ref()).unwrap();
            let mut con = client.get_connection().unwrap();

            // Add some key values for test.
            let keys = vec!["a", "b", "c", "1", "2"];
            for key in keys {
                let val = format!("{}_value", key);
                con.set::<_, _, ()>(key, val).unwrap();
            }
            Ok(redis)
        }
        Err(e) => Err(format!("Start {container_name} failed: {e}").into()),
    }
}

async fn run_mysql_server(docker: &Docker) -> Result<ContainerAsync<Mysql>> {
    let container_name = "mysqld".to_string();

    // Stop the container
    let _ = docker.stop_container(&container_name, None).await;
    let _ = docker.remove_container(&container_name, None).await;

    // Add a table for test.
    // CREATE TABLE test.user(
    //   id INT,
    //   name VARCHAR(100),
    //   age SMALLINT UNSIGNED,
    //   salary DOUBLE,
    //   active BOOL
    // );
    //
    // +------+-------+------+---------+--------+
    // | id   | name  | age  | salary  | active |
    // +------+-------+------+---------+--------+
    // |    1 | Alice |   24 |     100 |      1 |
    // |    2 | Bob   |   35 |   200.1 |      0 |
    // |    3 | Lily  |   41 |  1000.2 |      1 |
    // |    4 | Tom   |   55 | 3000.55 |      0 |
    // |    5 | NULL  | NULL |    NULL |   NULL |
    // +------+-------+------+---------+--------+
    let mysql_res = Mysql::default()
        .with_init_sql(
"CREATE TABLE test.user(id INT, name VARCHAR(100), age SMALLINT UNSIGNED, salary DOUBLE, active BOOL); INSERT INTO test.user VALUES(1, 'Alice', 24, 100, true), (2, 'Bob', 35, 200.1, false), (3, 'Lily', 41, 1000.2, true), (4, 'Tom', 55, 3000.55, false), (5, NULL, NULL, NULL, NULL);"
        .to_string()
        .into_bytes(),
)
        .with_network("host")
        .with_startup_timeout(Duration::from_secs(300))
        .with_container_name(&container_name)
        .start().await;

    match mysql_res {
        Ok(mysql) => Ok(mysql),
        Err(e) => Err(format!("Start {container_name} failed: {e}").into()),
    }
}
