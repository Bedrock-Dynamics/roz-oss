//! One-shot dev DB seed: creates a tenant + admin API key and prints the full key.
//!
//! Usage:
//! ```sh
//! DATABASE_URL=<url> cargo run -p roz-db --bin seed_dev
//! ```

#[tokio::main]
async fn main() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL required");
    let allow_non_dev = std::env::var("ROZ_SEED_DEV_ALLOW_NON_DEV")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let looks_dev = url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("-dev")
        || url.contains("_dev")
        || url.contains("dev.");
    if !looks_dev && !allow_non_dev {
        panic!(
            "seed_dev refused: DATABASE_URL does not look like a dev database; set ROZ_SEED_DEV_ALLOW_NON_DEV=1 to override"
        );
    }
    let pool = roz_db::create_pool(&url).await.expect("connect");
    roz_db::run_migrations(&pool).await.expect("migrate");

    let slug = format!("dev-e2e-{}", uuid::Uuid::new_v4().simple());
    let tenant = roz_db::tenant::create_tenant(&pool, "Dev E2E Tenant", &slug, "organization")
        .await
        .expect("create tenant");

    let result = roz_db::api_keys::create_api_key(&pool, tenant.id, "dev-e2e-key", &[], "seed")
        .await
        .expect("create api key");

    let print_full_key = std::env::var("ROZ_SEED_DEV_PRINT_FULL_KEY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if print_full_key {
        println!("{}", result.full_key);
    } else {
        eprintln!("API key created. Set ROZ_SEED_DEV_PRINT_FULL_KEY=1 to print the full key.");
    }
}
