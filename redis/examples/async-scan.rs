use futures::stream::StreamExt;
use redis::{AsyncCommands, AsyncIter};

#[tokio::main]
async fn main() -> redis::RedisResult<()> {
    let client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let mut con = client.get_multiplexed_async_connection().await?;

    let _: () = con.flushall().await?;
    let _: () = con.set("async-key1", b"foo").await?;
    let _: () = con.set("async-key2", b"foo").await?;

    let iter: AsyncIter<String> = con.scan().await?;
    #[cfg(not(feature = "safe_iterators"))]
    let mut keys: Vec<String> = iter.collect().await;
    #[cfg(feature = "safe_iterators")]
    let mut keys: Vec<_> = iter.map(std::result::Result::unwrap).collect().await;

    keys.sort();

    assert_eq!(
        keys,
        vec!["async-key1".to_string(), "async-key2".to_string()]
    );
    Ok(())
}
