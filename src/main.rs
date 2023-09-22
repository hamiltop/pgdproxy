use clap::Parser;

use pgdproxy::listener::{Config, Listener};

/// Postgres Debug Proxy
#[derive(Parser, Debug)]
struct Args {
    #[arg(short, long)]
    client_port: u16,
    #[arg(short, long)]
    target_address: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let client_port = args.client_port;
    let target_address = args.target_address;
    Listener::start(Config {
        port: client_port,
        target_address,
        ch: None,
        port_mapper: None,
    })
    .await
    .unwrap();
}

#[cfg(test)]
mod tests {
    use pgdproxy::listener::{self, Listener, PortMapper};
    use sqlx::{Connection, Executor, Row};
    use tokio::{sync::oneshot, task::JoinHandle};

    async fn setup(client_port: u16) -> (oneshot::Receiver<()>, JoinHandle<()>, PortMapper) {
        let target_address = "localhost:54320".to_string();
        let port_mapper = PortMapper::new();
        let (s, r) = oneshot::channel::<()>();
        let pm = port_mapper.clone();
        let listener = tokio::spawn(async move {
            let port_mapper = pm;
            let _r = Listener::start(listener::Config {
                port: client_port,
                target_address,
                ch: Some(s),
                port_mapper: Some(port_mapper),
            })
            .await;
        });
        (r, listener, port_mapper)
    }

    #[tokio::test]
    async fn test_forwarder_simple() {
        let client_port = 9876;
        let (r, listener, _) = setup(client_port).await;
        r.await.unwrap();

        let conn = sqlx::PgPool::connect(
            format!("postgresql://postgres:postgres@localhost:{client_port}/postgres").as_str(),
        )
        .await
        .unwrap();

        let r: i32 = sqlx::query_scalar("SELECT $1")
            .bind(1)
            .fetch_one(&conn)
            .await
            .unwrap();
        assert_eq!(r, 1);
        listener.abort();
    }

    #[tokio::test]
    async fn test_debugging() {
        let client_port = 8765;
        let value = 123843;
        let (r, listener, port_mapper) = setup(client_port).await;
        r.await.unwrap();

        // Only use one connection because when we specify a debug port we can't use multiple connections
        let mut conn = sqlx::PgConnection::connect(
            format!("postgresql://postgres:postgres@localhost:{client_port}/postgres").as_str(),
        )
        .await
        .unwrap();

        let mut txn = conn.begin().await.unwrap();
        sqlx::query("create table test_debugging (id int)")
            .execute(&mut *txn)
            .await
            .unwrap();
        sqlx::query("insert into test_debugging values ($1)")
            .bind(value)
            .execute(&mut *txn)
            .await
            .unwrap();

        let r: i32 = sqlx::query_scalar("select id from test_debugging")
            .fetch_one(&mut *txn)
            .await
            .unwrap();
        assert_eq!(r, value);

        // now try via the debug port
        // We can't reliably get the client port to do the lookup, but in this
        // test we should only see one debug port so we can use that
        let debug_ports = port_mapper.get_all_debug_ports().await;
        assert_eq!(debug_ports.len(), 1);
        let debug_port = debug_ports[0];
        let mut conn = sqlx::PgConnection::connect(&format!(
            "postgresql://postgres:postgres@localhost:{debug_port}/postgres",
        ))
        .await
        .unwrap();

        // Have to use raw executor to avoid prepared statements
        let r = conn
            .fetch_one("select id from test_debugging")
            .await
            .unwrap();
        let r: i32 = r.get(0);
        assert_eq!(r, value);

        // disconnect
        drop(conn);

        let r: i32 = sqlx::query_scalar("select id from test_debugging")
            .fetch_one(&mut *txn)
            .await
            .unwrap();
        assert_eq!(r, value);

        txn.rollback().await.unwrap();

        listener.abort();
    }

    #[tokio::test]
    async fn test_forwarder_concurrently() {
        let client_port = 7654;
        let (r, listener, _) = setup(client_port).await;

        r.await.unwrap();

        let (tx, rx) = async_channel::unbounded::<()>();
        let (tx2, rx2) = async_channel::unbounded::<()>();
        for _ in 0..10 {
            let rx = rx.clone();
            let tx2 = tx2.clone();
            tokio::spawn(async move {
                loop {
                    let r = rx.recv().await;
                    let conn = sqlx::PgPool::connect(
                        format!("postgresql://postgres:postgres@localhost:{client_port}/postgres")
                            .as_str(),
                    )
                    .await
                    .unwrap();
                    match r {
                        Ok(_) => {
                            let r: i32 = sqlx::query_scalar("SELECT $1")
                                .bind(1)
                                .fetch_one(&conn)
                                .await
                                .unwrap();
                            assert_eq!(r, 1);
                        }
                        Err(e) => {
                            dbg!(e);
                            break;
                        }
                    }
                    tx2.send(()).await.unwrap();
                }
            });
        }
        for _ in 0..1000 {
            tx.send(()).await.unwrap();
        }
        for _ in 0..1000 {
            rx2.recv().await.unwrap();
        }
        listener.abort();
    }
}
