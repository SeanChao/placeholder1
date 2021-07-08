mod cache;
mod error;

#[tokio::main]
async fn main() {
    let redis_client =
        redis::Client::open("redis://127.0.0.1/").expect("failed to initialize redis client");
    let api = filters::root(&redis_client);
    warp::serve(api).run(([127, 0, 0, 1], 9000)).await;
}

mod filters {
    use super::handlers;
    use std::convert::Infallible;
    use warp::Filter;
    type Client = redis::Client;

    pub fn root(
        redis_client: &redis::Client,
    ) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        pypi_index().or(pypi_packages(redis_client.clone()))
    }

    // GET /pypi/web/simple/:string
    fn pypi_index() -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path!("pypi" / "web" / "simple" / String).and_then(handlers::get_pypi_index)
    }

    // GET /pypi/package/:string/:string/:string/:string
    fn pypi_packages(
        client: Client,
    ) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path!("pypi" / "packages" / String / String / String / String)
            .and(with_redis_client(client))
            .and_then(handlers::get_pypi_pkg)
    }

    fn with_redis_client(
        client: redis::Client,
    ) -> impl Filter<Extract = (redis::Client,), Error = Infallible> + Clone {
        warp::any().map(move || client.clone())
    }
}

mod handlers {
    use super::models;
    use crate::cache::CacheEntry;
    use reqwest::ClientBuilder;
    use std::fs;
    use std::io::prelude::*;
    use warp::{http::Response, Rejection};

    type Client = redis::Client;

    pub async fn get_pypi_index(path: String) -> Result<impl warp::Reply, Rejection> {
        let upstream = format!("https://pypi.org/simple/{}", path);
        let client = ClientBuilder::new().build().unwrap();
        let resp = client.get(&upstream).send().await;
        match resp {
            Ok(response) => Ok(Response::builder()
                .header("content-type", "text/html")
                .body(response.text().await.unwrap().replace(
                    "https://files.pythonhosted.org/packages",
                    format!("http://localhost:9000/pypi/packages").as_str(),
                ))),
            Err(err) => {
                println!("{:?}", err);
                Err(warp::reject::reject())
            }
        }
    }

    pub async fn get_pypi_pkg(
        path: String,
        path2: String,
        path3: String,
        path4: String,
        redis_client: Client,
    ) -> Result<impl warp::Reply, Rejection> {
        let parent_dirs = format!("{}/{}/{}", path, path2, path3);
        let filename = format!("{}", path4);
        let fullpath = format!("{}/{}/{}/{}", path, path2, path3, path4);
        let cached_file_path = format!("cache/{}/{}", parent_dirs, filename);

        let mut con = models::get_con(redis_client)
            .await
            .map_err(|e| warp::reject::custom(e))
            .unwrap();

        // Check whether the file is cached
        let cache_result = models::get_cache_entry(&mut con, &fullpath).await?;
        println!("[HMGET] {} -> {:?}", &fullpath, &cache_result);
        if let Some(cache_entry) = cache_result {
            // cache hit
            println!("cache hit");
            let cached_file_path = format!("cache/{}", cache_entry.path);
            println!("read file @{}", cached_file_path);
            let file_content = match fs::read(cached_file_path) {
                Ok(data) => data,
                Err(_) => vec![],
            };
            if file_content.len() > 0 {
                return Ok(Response::builder().body(file_content));
            }
        }
        // cache miss
        println!("cache miss");
        let upstream = format!("https://files.pythonhosted.org/packages/{}", fullpath);
        let client = ClientBuilder::new().build().unwrap();
        println!("GET {}", &upstream);
        let resp = client.get(&upstream).send().await;
        match resp {
            Ok(response) => {
                println!("fetched {}", response.content_length().unwrap());
                // cache to local filesystem
                let resp_bytes = response.bytes().await.unwrap();
                let data_to_write = resp_bytes.to_vec();
                fs::create_dir_all(format!("cache/{}", parent_dirs)).unwrap();
                let mut f = fs::File::create(&cached_file_path).unwrap();
                f.write_all(&data_to_write).unwrap();
                let redis_resp_str =
                    models::set_cache_entry(&mut con, &fullpath, &CacheEntry::new(&fullpath))
                        .await?;
                println!("[HMSET] respond: {}", redis_resp_str);
                Ok(Response::builder().body(data_to_write))
            }
            Err(err) => {
                println!("{:?}", err);
                Err(warp::reject::reject())
            }
        }
    }
}

mod models {
    extern crate redis;
    use crate::cache::CacheEntry;
    use crate::error::Error::*;
    use crate::error::Result;
    use redis::{aio::Connection, AsyncCommands};
    use std::collections::HashMap;

    pub async fn get_con(client: redis::Client) -> Result<Connection> {
        client
            .get_async_connection()
            .await
            .map_err(|e| RedisClientError(e).into())
    }

    // get cache entry
    pub async fn get_cache_entry(con: &mut Connection, key: &str) -> Result<Option<CacheEntry>> {
        let map: HashMap<String, String> = con.hgetall(key).await?;
        println!("redis return {:?}", map);
        if map.is_empty() {
            // not exist in cache
            return Ok(None);
        }
        let cache_entry = CacheEntry {
            valid: if map.get("valid").unwrap_or(&String::from("0")).eq(&"0") {
                false
            } else {
                true
            },
            path: String::from(map.get("path").unwrap_or(&String::from(""))),
        };
        Ok(Some(cache_entry))
    }

    pub async fn set_cache_entry(
        con: &mut Connection,
        key: &str,
        entry: &CacheEntry,
    ) -> Result<String> {
        let kv_array = entry.to_redis_multiple_fields();
        match con
            .hset_multiple::<&str, &str, &str, String>(key, &kv_array)
            .await
        {
            Ok(s) => Ok(s),
            Err(e) => Err(RedisCMDError(e)),
        }
    }
}
