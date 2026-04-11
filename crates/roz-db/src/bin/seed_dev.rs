//! One-shot dev DB seed: creates a tenant + admin API key and prints the full key.
//!
//! Usage:
//! ```sh
//! DATABASE_URL=<url> cargo run -p roz-db --bin seed_dev
//! ```

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    let pool = roz_db::create_pool(&url).await.expect("connect");
    roz_db::run_migrations(&pool).await.expect("migrate");

    let slug = format!("dev-e2e-{}", uuid::Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(&pool, "Dev E2E Tenant", &slug, "organization")
        .await
        .expect("create tenant");

    let result = roz_db::api_keys::create_api_key(&pool, tenant.id, "dev-e2e-key", &[], "seed")
        .await
        .expect("create api key");

    println!("{}", result.full_key);
}
