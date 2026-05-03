use std::env;

use testcontainers::GenericImage;
use testcontainers::ImageExt;
use testcontainers::core::{ContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;

const POSTGRES_PORT: ContainerPort = ContainerPort::Tcp(5432);

fn reserve_host_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve host port")
        .local_addr()
        .expect("reserved port local addr")
        .port()
}

/// Guard that holds a running Postgres container. The container is stopped and
/// removed when this guard is dropped.
pub struct PgGuard {
    _container: Option<testcontainers::ContainerAsync<GenericImage>>,
    url: String,
}

impl PgGuard {
    /// Connection URL for the running Postgres instance.
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Starts a fresh Postgres testcontainer and returns a guard that owns it.
///
/// If `DATABASE_URL` is set, returns a guard with no container (uses the
/// external database). The container is removed when the guard is dropped.
pub async fn pg_container() -> PgGuard {
    if let Ok(url) = env::var("DATABASE_URL") {
        return PgGuard { _container: None, url };
    }

    let host_port = reserve_host_port();
    let container = GenericImage::new("postgres", "16-alpine")
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_wait_for(WaitFor::message_on_stdout(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_DB", "roz_test")
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_PASSWORD", "test")
        .with_cmd(["-c", "fsync=off"])
        .with_mapped_port(host_port, POSTGRES_PORT)
        .start()
        .await
        .expect("failed to start Postgres testcontainer");

    let host = container.get_host().await.expect("failed to get host");
    let port = host_port;

    let url = format!("postgres://postgres:test@{host}:{port}/roz_test");

    PgGuard {
        _container: Some(container),
        url,
    }
}

/// Returns a Postgres connection URL, starting a testcontainer if needed.
///
/// **Deprecated**: prefer `pg_container()` which returns an owned guard that
/// cleans up the container on drop. This function leaks the container handle
/// via a `static OnceCell`.
pub async fn pg_url() -> &'static str {
    use tokio::sync::OnceCell;

    // Store both the guard (keeps container alive) and the leaked URL
    // (avoids leaking a new String on every call).
    static PG: OnceCell<(PgGuard, &'static str)> = OnceCell::const_new();

    let (_, url) = PG
        .get_or_init(|| async {
            let guard = pg_container().await;
            let url: &'static str = Box::leak(guard.url.clone().into_boxed_str());
            (guard, url)
        })
        .await;
    url
}
