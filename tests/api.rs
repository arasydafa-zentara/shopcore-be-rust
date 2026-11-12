//! End-to-end tests against a running stack (`docker compose up -d --build`).
//! Set BASE_URL (default http://localhost:8000). If nothing is listening the tests are skipped.

use reqwest::{Client, Response};
use serde_json::{Value, json};

fn tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn client() -> Client {
    tls();
    Client::new()
}

fn base() -> String {
    std::env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:8000".into())
}

async fn up(c: &Client) -> bool {
    tls();
    let ok = c
        .get(format!("{}/health", base()))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    if !ok {
        eprintln!(
            "SKIPPED: no service at {} (run `docker compose up -d --build` first)",
            base()
        );
    }
    ok
}

async fn call(c: &Client, method: &str, path: &str, tok: Option<&str>, body: Option<Value>) -> Response {
    let mut r = c.request(method.parse().unwrap(), format!("{}{}", base(), path));
    if let Some(t) = tok {
        r = r.bearer_auth(t);
    }
    if let Some(b) = body {
        r = r.json(&b);
    }
    r.send().await.unwrap()
}

async fn user(c: &Client) -> (i64, String) {
    let n = format!("t_{}", rand::random::<u32>());
    let r: Value = call(
        c,
        "POST",
        "/api/register",
        None,
        Some(json!({"username": n, "email": format!("{n}@x.com"), "password": "pw"})),
    )
    .await
    .json()
    .await
    .unwrap();
    let t: Value = call(
        c,
        "POST",
        "/api/login",
        None,
        Some(json!({"username": n, "password": "pw"})),
    )
    .await
    .json()
    .await
    .unwrap();
    (r["id"].as_i64().unwrap(), t["token"].as_str().unwrap().to_string())
}

async fn admin(c: &Client) -> String {
    let t: Value = call(
        c,
        "POST",
        "/api/login",
        None,
        Some(json!({"username": "admin", "password": "admin123"})),
    )
    .await
    .json()
    .await
    .unwrap();
    t["token"].as_str().unwrap().to_string()
}

async fn product(c: &Client, adm: &str, price: f64, stock: i64) -> i64 {
    let p: Value = call(
        c,
        "POST",
        "/api/products",
        Some(adm),
        Some(json!({"sku": format!("S{}", rand::random::<u32>()), "name": "t", "price": price, "stock": stock})),
    )
    .await
    .json()
    .await
    .unwrap();
    p["id"].as_i64().unwrap()
}

#[tokio::test]
async fn admin_routes_fail_closed_and_errors_are_json() {
    let c = client();
    if !up(&c).await {
        return;
    }
    assert_eq!(call(&c, "GET", "/api/admin/users", None, None).await.status(), 401);
    assert_eq!(call(&c, "GET", "/api/reports/sales", None, None).await.status(), 401);
    let (_, tok) = user(&c).await;
    assert_eq!(
        call(&c, "GET", "/api/admin/users", Some(&tok), None).await.status(),
        403
    );
    let r = call(&c, "GET", "/api/nope", None, None).await;
    assert_eq!(r.status(), 404);
    assert!(r.json::<Value>().await.unwrap().get("error").is_some());
}

#[tokio::test]
async fn injection_and_traversal_are_rejected() {
    let c = client();
    if !up(&c).await {
        return;
    }
    let adm = admin(&c).await;
    assert_eq!(
        call(&c, "GET", "/api/products?sort=id;drop%20table%20users", None, None)
            .await
            .status(),
        400
    );
    assert_eq!(
        call(&c, "GET", "/api/reports/sales?group_by=1", Some(&adm), None)
            .await
            .status(),
        400
    );
    assert_eq!(
        call(
            &c,
            "GET",
            "/api/reports/download?file=../../etc/passwd",
            Some(&adm),
            None
        )
        .await
        .status(),
        404
    );
    assert_eq!(
        call(
            &c,
            "POST",
            "/api/reports/export",
            Some(&adm),
            Some(json!({"filename": "../x"}))
        )
        .await
        .status(),
        400
    );
    let (_, tok) = user(&c).await;
    call(&c, "PATCH", "/api/users/me", Some(&tok), Some(json!({"role": "admin"}))).await;
    let me: Value = call(&c, "GET", "/api/users/me", Some(&tok), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["role"], "user");
}

#[tokio::test]
async fn stock_is_never_oversold_under_concurrency() {
    let c = client();
    if !up(&c).await {
        return;
    }
    let adm = admin(&c).await;
    let pid = product(&c, &adm, 10.0, 1).await;
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let c = c.clone();
        tasks.push(tokio::spawn(async move {
            let (_, t) = user(&c).await;
            call(
                &c,
                "POST",
                "/api/orders",
                Some(&t),
                Some(json!({"items": [{"product_id": pid, "quantity": 1}]})),
            )
            .await
            .status()
            .as_u16()
        }));
    }
    let mut codes = Vec::new();
    for t in tasks {
        codes.push(t.await.unwrap());
    }
    assert_eq!(codes.iter().filter(|c| **c == 201).count(), 1, "{codes:?}");
    let p: Value = call(&c, "GET", &format!("/api/products/{pid}"), None, None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(p["stock"], 0);
}

#[tokio::test]
async fn wallet_cannot_be_double_spent_and_refunds_are_capped() {
    let c = client();
    if !up(&c).await {
        return;
    }
    let adm = admin(&c).await;
    let pid = product(&c, &adm, 60.0, 50).await;
    let (_, tok) = user(&c).await;
    call(
        &c,
        "POST",
        "/api/wallet/topup",
        Some(&tok),
        Some(json!({"amount": 100})),
    )
    .await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        let o: Value = call(
            &c,
            "POST",
            "/api/orders",
            Some(&tok),
            Some(json!({"items": [{"product_id": pid, "quantity": 1}]})),
        )
        .await
        .json()
        .await
        .unwrap();
        ids.push(o["id"].as_i64().unwrap());
    }
    let mut tasks = Vec::new();
    for id in ids.clone() {
        let (c, tok) = (c.clone(), tok.clone());
        tasks.push(tokio::spawn(async move {
            call(
                &c,
                "POST",
                &format!("/api/orders/{id}/pay"),
                Some(&tok),
                Some(json!({"method": "wallet"})),
            )
            .await
            .status()
            .as_u16()
        }));
    }
    let mut codes = Vec::new();
    for t in tasks {
        codes.push(t.await.unwrap());
    }
    assert_eq!(codes.iter().filter(|c| **c == 200).count(), 1, "{codes:?}");
    let w: Value = call(&c, "GET", "/api/wallet", Some(&tok), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(w["balance"], 40.0);

    let paid = ids[codes.iter().position(|c| *c == 200).unwrap()];
    assert_eq!(
        call(
            &c,
            "POST",
            &format!("/api/orders/{paid}/refund"),
            Some(&tok),
            Some(json!({"amount": 25}))
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        call(
            &c,
            "POST",
            &format!("/api/orders/{paid}/refund"),
            Some(&tok),
            Some(json!({"amount": 60}))
        )
        .await
        .status(),
        400
    );
    assert_eq!(
        call(
            &c,
            "POST",
            &format!("/api/orders/{paid}/refund"),
            Some(&tok),
            Some(json!({}))
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        call(
            &c,
            "POST",
            &format!("/api/orders/{paid}/refund"),
            Some(&tok),
            Some(json!({}))
        )
        .await
        .status(),
        409
    );
}

#[tokio::test]
async fn orders_are_private_and_bad_amounts_rejected() {
    let c = client();
    if !up(&c).await {
        return;
    }
    let adm = admin(&c).await;
    let pid = product(&c, &adm, 5.0, 10).await;
    let (_, a) = user(&c).await;
    let (_, b) = user(&c).await;
    let o: Value = call(
        &c,
        "POST",
        "/api/orders",
        Some(&a),
        Some(json!({"items": [{"product_id": pid, "quantity": 1}]})),
    )
    .await
    .json()
    .await
    .unwrap();
    let id = o["id"].as_i64().unwrap();
    assert_eq!(
        call(&c, "GET", &format!("/api/orders/{id}"), Some(&b), None)
            .await
            .status(),
        404
    );
    assert_eq!(
        call(&c, "POST", &format!("/api/orders/{id}/cancel"), Some(&b), None)
            .await
            .status(),
        404
    );
    assert_eq!(
        call(&c, "POST", "/api/wallet/topup", Some(&a), Some(json!({"amount": -1})))
            .await
            .status(),
        400
    );
    assert_eq!(
        call(
            &c,
            "POST",
            "/api/orders",
            Some(&a),
            Some(json!({"items": [{"product_id": pid, "quantity": -2}]}))
        )
        .await
        .status(),
        400
    );
}

#[tokio::test]
async fn failed_logins_are_throttled_per_username() {
    let c = client();
    if !up(&c).await {
        return;
    }
    let (_, _) = user(&c).await;
    let name = format!("nobody_{}", rand::random::<u32>());
    let mut codes = Vec::new();
    for _ in 0..12 {
        codes.push(
            call(
                &c,
                "POST",
                "/api/login",
                None,
                Some(json!({"username": name, "password": "x"})),
            )
            .await
            .status()
            .as_u16(),
        );
    }
    assert_eq!(&codes[..10], &[401; 10]);
    assert_eq!(codes[10], 429);
    // other accounts are not affected
    assert_eq!(
        call(
            &c,
            "POST",
            "/api/login",
            None,
            Some(json!({"username": "admin", "password": "admin123"}))
        )
        .await
        .status(),
        200
    );
}

#[tokio::test]
async fn spreadsheet_formulas_are_neutralized_in_exports() {
    use std::io::Read;
    let c = client();
    if !up(&c).await {
        return;
    }
    let adm = admin(&c).await;
    let p: Value = call(
        &c,
        "POST",
        "/api/products",
        Some(&adm),
        Some(json!({"sku": "F1", "name": "=HYPERLINK(1)", "price": 1, "stock": 5})),
    )
    .await
    .json()
    .await
    .unwrap();
    let (_, tok) = user(&c).await;
    call(
        &c,
        "POST",
        "/api/orders",
        Some(&tok),
        Some(json!({"items": [{"product_id": p["id"], "quantity": 1}]})),
    )
    .await;
    let file = format!("t{}.csv", rand::random::<u32>());
    call(
        &c,
        "POST",
        "/api/reports/export",
        Some(&adm),
        Some(json!({"filename": file})),
    )
    .await;
    let bytes = call(
        &c,
        "GET",
        &format!("/api/reports/download?file={file}.gz"),
        Some(&adm),
        None,
    )
    .await
    .bytes()
    .await
    .unwrap();
    let mut text = String::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap();
    assert!(text.contains("'=HYPERLINK(1)"), "{text}");
    assert!(!text.contains(",=HYPERLINK"));
}

#[tokio::test]
async fn order_cannot_be_cancelled_while_a_bank_payment_is_in_flight() {
    let c = client();
    if !up(&c).await {
        return;
    }
    let bank = std::env::var("BANK_ADMIN_URL").unwrap_or_else(|_| "http://localhost:5001".into());
    if c.post(format!("{bank}/config"))
        .json(&json!({"latency_ms": 1500, "drop_rate": 0}))
        .send()
        .await
        .is_err()
    {
        eprintln!("SKIPPED: bank simulator not reachable at {bank}");
        return;
    }
    let adm = admin(&c).await;
    let pid = product(&c, &adm, 10.0, 10).await;
    let (_, tok) = user(&c).await;
    let o: Value = call(
        &c,
        "POST",
        "/api/orders",
        Some(&tok),
        Some(json!({"items": [{"product_id": pid, "quantity": 1}]})),
    )
    .await
    .json()
    .await
    .unwrap();
    let id = o["id"].as_i64().unwrap();
    let paying = {
        let (c, tok) = (c.clone(), tok.clone());
        tokio::spawn(async move {
            call(
                &c,
                "POST",
                &format!("/api/orders/{id}/pay"),
                Some(&tok),
                Some(json!({"method": "bank"})),
            )
            .await
            .status()
            .as_u16()
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        call(&c, "POST", &format!("/api/orders/{id}/cancel"), Some(&tok), None)
            .await
            .status(),
        409
    );
    assert_eq!(
        call(
            &c,
            "POST",
            &format!("/api/orders/{id}/pay"),
            Some(&tok),
            Some(json!({"method": "bank"}))
        )
        .await
        .status(),
        409
    );
    assert_eq!(paying.await.unwrap(), 200);
    let o: Value = call(&c, "GET", &format!("/api/orders/{id}"), Some(&tok), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(o["status"], "paid");
    let _ = c
        .post(format!("{bank}/config"))
        .json(&json!({"latency_ms": 0}))
        .send()
        .await;
}
