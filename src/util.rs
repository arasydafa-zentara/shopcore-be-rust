//! Coercion helpers that mimic the legacy `to_int` / `to_money` / `clean`, the lenient JSON body
//! extractor and the Flask-style timestamp format.

use axum::{
    body::Bytes,
    extract::{FromRequest, Request},
};
use serde_json::{Map, Value};

use crate::error::AppError;

/// Python `int(v)` with the legacy fallback to a leading integer (`"12abc"` -> 12).
pub fn to_int(v: Option<&Value>, default: i64) -> i64 {
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64))
            .unwrap_or(default),
        Some(Value::Bool(b)) => *b as i64,
        Some(Value::String(s)) => parse_int_str(s).unwrap_or(default),
        _ => default,
    }
}

pub fn parse_int_str(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

pub fn parse_money(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Bool(b)) => Some(*b as i64 as f64),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Python `str()` of a JSON scalar; `None` for null/absent/containers.
pub fn py_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(if *b { "True" } else { "False" }.to_string()),
        _ => None,
    }
}

/// Legacy `clean()`: missing -> "", otherwise the stripped string.
pub fn clean(v: Option<&Value>) -> String {
    py_str(v).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Round to 2 decimals the way Python's `round(x, 2)` does (exact decimal expansion, ties to even).
pub fn round2(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.2}").parse().unwrap_or(x)
}

/// Python `str(float)` for CSV output (`100.0`, `89.99`).
pub fn py_float(x: f64) -> String {
    format!("{x:?}")
}

/// Parse a path segment the way Flask's `<int:id>` does: digits only, otherwise 404.
pub fn parse_id(s: &str) -> Result<i32, AppError> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AppError::not_found());
    }
    s.parse::<i32>().map_err(|_| AppError::not_found())
}

/// SQL expression rendering a `TIMESTAMP` column the way Flask's JSON provider renders datetimes.
pub fn ts(col: &str) -> String {
    format!("to_char({col}, 'Dy, DD Mon YYYY HH24:MI:SS') || ' GMT'")
}

/// Page/limit as in the legacy list endpoints: limit < 0 means "no limit", page < 1 is clamped.
/// Hard ceiling for one page. `limit < 0` keeps its legacy meaning of "everything", which is capped here as well so a
/// single request can never dump an unbounded table.
pub const MAX_PAGE: i64 = 10_000;

pub fn paging(page: Option<&str>, limit: Option<&str>) -> (i64, Option<i64>) {
    let page = page.and_then(parse_int_str).unwrap_or(1).max(1);
    let limit = limit.and_then(parse_int_str).unwrap_or(20);
    if limit < 0 {
        (0, Some(MAX_PAGE))
    } else {
        let limit = limit.min(MAX_PAGE);
        ((page - 1).saturating_mul(limit), Some(limit))
    }
}

/// JSON body extractor modelled on Flask's `request.get_json(silent=True) or {}`: a missing body, or one that is not
/// declared as JSON, is an empty object. A body that IS declared as JSON but cannot be parsed (or is not an object) is
/// rejected with 400: silently treating a truncated `{"amount": 5` as "no amount" would trigger the endpoint defaults
/// (a full refund, a role change, a wallet payment) that the caller never asked for.
pub struct JsonBody(pub Map<String, Value>);

impl<S: Send + Sync> FromRequest<S> for JsonBody {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let declared_json = req
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase().contains("json"))
            .unwrap_or(false);
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|_| AppError::with(axum::http::StatusCode::PAYLOAD_TOO_LARGE, "body too large"))?;
        let map = match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(m)) => m,
            _ if declared_json && !bytes.iter().all(u8::is_ascii_whitespace) => {
                return Err(AppError::bad_request("request body must be a JSON object"));
            }
            _ => Map::new(),
        };
        Ok(JsonBody(map))
    }
}

pub fn finite_positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn int_coercion_matches_legacy() {
        assert_eq!(to_int(Some(&json!("12abc")), 0), 12);
        assert_eq!(to_int(Some(&json!("  -7")), 0), -7);
        assert_eq!(to_int(Some(&json!(3.9)), 0), 3);
        assert_eq!(to_int(Some(&json!(null)), 5), 5);
        assert_eq!(to_int(Some(&json!("abc")), 9), 9);
        assert_eq!(to_int(None, 1), 1);
    }

    #[test]
    fn money_coercion() {
        assert_eq!(parse_money(Some(&json!("12.5"))), Some(12.5));
        assert_eq!(parse_money(Some(&json!("x"))), None);
        assert!(parse_money(Some(&json!("nan"))).unwrap().is_nan());
    }

    #[test]
    fn rounding_matches_python() {
        assert_eq!(round2(179.98 * 0.9), 161.98);
        assert_eq!(round2(2.675), 2.67); // Python: round(2.675, 2) == 2.67
        assert_eq!(round2(0.125), 0.12); // exact tie -> even
        assert_eq!(py_float(100.0), "100.0");
    }

    #[test]
    fn ids_and_paging() {
        assert!(parse_id("12").is_ok());
        assert!(parse_id("-1").is_err());
        assert!(parse_id("abc").is_err());
        assert_eq!(paging(None, None), (0, Some(20)));
        assert_eq!(paging(Some("3"), Some("10")), (20, Some(10)));
        assert_eq!(paging(Some("0"), Some("-1")), (0, Some(MAX_PAGE)));
        assert_eq!(paging(None, Some("999999999")), (0, Some(MAX_PAGE)));
    }
}
