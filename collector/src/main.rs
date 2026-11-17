//! 국토교통부 실거래가 수집기.
//!
//! 사용: `DATA_GO_KR_KEY=... rt-collect [출력폴더]`
//!
//! - 인증키는 환경변수로만 받고, 어떤 출력에도 찍지 않는다.
//! - 최근 N개월(기본 24)을 매번 다시 받아 `<출력폴더>/<시군구코드>.json` 으로 저장한다.
//! - 받지 못한 달은 이전 파일의 값을 그대로 둔다. 아무것도 못 받으면 파일을 건드리지 않고 실패로 끝난다.

mod parse;

use parse::{amount, area, int, parse_page, pick, Item, Page};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, process, thread};

const BASE: &str = "https://apis.data.go.kr/1613000/";
const ROWS: usize = 1000;
const MAX_PAGES: usize = 60;
const MAX_BODY: u64 = 32 * 1024 * 1024;
const PACE: Duration = Duration::from_millis(120);

/// "사이트 시군구코드=조회코드|예비코드;..." — 조회코드는 법정동 코드 앞 5자리.
/// 광주 5개 구. 행정구역 통합 전후 코드가 달라 둘 다 시도한다.
const DEFAULT_REGIONS: &str =
    "12270=12270|29155;12210=12210|29110;12240=12240|29140;12300=12300|29170;12330=12330|29200";

struct Kind {
    id: &'static str,
    label: &'static str,
    /// 같은 자료의 다른 판(상세/일반). 앞에서부터 시도한다.
    endpoints: &'static [&'static str],
    cols: &'static [&'static str],
}

const KINDS: [Kind; 3] = [
    Kind {
        id: "apt",
        label: "아파트 매매",
        endpoints: &[
            "RTMSDataSvcAptTradeDev/getRTMSDataSvcAptTradeDev",
            "RTMSDataSvcAptTrade/getRTMSDataSvcAptTrade",
        ],
        cols: &["ym", "d", "umd", "name", "area", "floor", "built", "amt", "cancel", "direct", "jibun"],
    },
    Kind {
        id: "rent",
        label: "아파트 전월세",
        endpoints: &["RTMSDataSvcAptRent/getRTMSDataSvcAptRent"],
        cols: &["ym", "d", "umd", "name", "area", "floor", "built", "dep", "mon", "renew", "jibun"],
    },
    Kind {
        id: "nrg",
        label: "상업업무용 매매",
        endpoints: &["RTMSDataSvcNrgTrade/getRTMSDataSvcNrgTrade"],
        cols: &[
            "ym", "d", "umd", "jibun", "use", "btype", "landuse", "bar", "par", "floor", "built", "amt",
            "cancel", "share", "direct",
        ],
    },
];

#[derive(Debug)]
enum FetchErr {
    /// 키가 틀렸거나 이 자료를 활용신청하지 않음
    Auth(String),
    /// 하루 호출 한도 초과
    Limit(String),
    Other(String),
}

struct Region {
    sgg: String,
    lawd: Vec<String>,
}

struct Ctx {
    agent: ureq::Agent,
    key_enc: String,
    key_raw: String,
    calls: u32,
}

impl Ctx {
    /// 어떤 문자열이든 밖으로 내보내기 전에 키를 지운다.
    fn redact(&self, s: &str) -> String {
        let mut out = s.replace(&self.key_enc, "***");
        if !self.key_raw.is_empty() {
            out = out.replace(&self.key_raw, "***");
        }
        out
    }
    fn log(&self, s: &str) {
        println!("{}", self.redact(s));
    }
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// 포털이 주는 키는 "인코딩"판과 "디코딩"판 두 가지다. 어느 쪽이 와도 주소에 넣을 수 있게 맞춘다.
fn encode_key(k: &str) -> String {
    let k = k.trim();
    let b = k.as_bytes();
    let mut already = k.contains('%');
    let mut i = 0;
    while already && i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() || !(b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit()) {
                already = false;
            }
            i += 3;
        } else {
            if !is_unreserved(b[i]) {
                already = false;
            }
            i += 1;
        }
    }
    if already {
        k.to_string()
    } else {
        percent_encode(k)
    }
}

fn parse_regions(spec: &str) -> Result<Vec<Region>, String> {
    let five = |s: &str| s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit());
    let mut out = Vec::new();
    for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (sgg, codes) = part.split_once('=').unwrap_or((part, part));
        let sgg = sgg.trim();
        let lawd: Vec<String> = codes.split('|').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
        if !five(sgg) || lawd.is_empty() || !lawd.iter().all(|c| five(c)) {
            return Err(format!("지역 설정이 잘못됐습니다(5자리 숫자 코드만): {part}"));
        }
        out.push(Region { sgg: sgg.to_string(), lawd });
    }
    if out.is_empty() {
        return Err("지역 설정이 비어 있습니다".into());
    }
    Ok(out)
}

/// 1970-01-01 기준 일수 → (년, 월, 일)
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn kst_today() -> (i64, u32, u32) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    civil_from_days((secs + 9 * 3600).div_euclid(86_400))
}

/// 이번 달을 포함해 n개월, 오래된 달부터.
fn months_back(y: i64, m: u32, n: u32) -> Vec<String> {
    let mut out = Vec::new();
    let base = y * 12 + i64::from(m) - 1;
    for i in (0..i64::from(n)).rev() {
        let t = base - i;
        out.push(format!("{:04}{:02}", t.div_euclid(12), t.rem_euclid(12) + 1));
    }
    out
}

fn describe(e: &ureq::Error) -> &'static str {
    // 오류 원문에는 요청 주소(키 포함)가 섞일 수 있어 종류만 남긴다.
    match e {
        ureq::Error::Timeout(_) => "시간 초과",
        ureq::Error::Io(_) => "네트워크 오류",
        ureq::Error::HostNotFound => "주소를 찾지 못함",
        ureq::Error::ConnectionFailed => "연결 실패",
        _ => "요청 실패",
    }
}

fn fetch_page(cx: &mut Ctx, endpoint: &str, lawd: &str, ym: &str, page_no: usize) -> Result<Page, FetchErr> {
    let url = format!(
        "{BASE}{endpoint}?serviceKey={}&LAWD_CD={lawd}&DEAL_YMD={ym}&pageNo={page_no}&numOfRows={ROWS}",
        cx.key_enc
    );
    let mut last = String::from("응답 없음");
    for attempt in 0..3u64 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(1500 * attempt));
        }
        cx.calls += 1;
        let mut resp = match cx.agent.get(&url).call() {
            Ok(r) => r,
            Err(e) => {
                last = describe(&e).to_string();
                continue;
            }
        };
        let status = resp.status().as_u16();
        let body = resp.body_mut().with_config().limit(MAX_BODY).read_to_string();
        match status {
            401 | 403 => return Err(FetchErr::Auth(format!("HTTP {status}"))),
            429 => {
                last = "HTTP 429".into();
                continue;
            }
            500..=599 => {
                last = format!("HTTP {status}");
                continue;
            }
            200..=299 => {}
            _ => return Err(FetchErr::Other(format!("HTTP {status}"))),
        }
        let text = match body {
            Ok(t) => t,
            Err(_) => {
                last = "본문 읽기 실패".into();
                continue;
            }
        };
        let page = match parse_page(&text) {
            Ok(p) => p,
            Err(e) => {
                let low = text.to_ascii_lowercase();
                if low.contains("unauthorized") || low.contains("forbidden") || low.contains("not_registered") {
                    return Err(FetchErr::Auth("인증 거부".into()));
                }
                last = e;
                continue;
            }
        };
        if page.ok() || page.no_data() {
            return Ok(page);
        }
        let why = format!("코드 {} {}", page.code, page.msg);
        return Err(match page.code.as_str() {
            "20" | "30" | "31" | "32" => FetchErr::Auth(why),
            "22" => FetchErr::Limit(why),
            _ => FetchErr::Other(why),
        });
    }
    Err(FetchErr::Other(last))
}

fn fetch_month(cx: &mut Ctx, endpoint: &str, lawd: &str, ym: &str) -> Result<Vec<Item>, FetchErr> {
    let mut all: Vec<Item> = Vec::new();
    for page_no in 1..=MAX_PAGES {
        let p = fetch_page(cx, endpoint, lawd, ym, page_no)?;
        let n = p.items.len();
        let total = p.total.unwrap_or(0) as usize;
        all.extend(p.items);
        thread::sleep(PACE);
        if n == 0 || n < ROWS || (total > 0 && all.len() >= total) {
            break;
        }
    }
    Ok(all)
}

fn opt_num<T: Into<Value>>(v: Option<T>) -> Value {
    v.map(Into::into).unwrap_or(Value::Null)
}

fn flag(on: bool) -> Value {
    Value::from(u8::from(on))
}

/// 응답 항목 하나를 사이트용 한 줄로 바꾼다. 금액이 없으면 버린다.
fn to_row(kind: &str, ym: &str, it: &Item) -> Option<Value> {
    let day = int(pick(it, &["dealDay", "일"])).filter(|d| (1..=31).contains(d));
    let umd = pick(it, &["umdNm", "법정동"]);
    let floor = int(pick(it, &["floor", "층"]));
    let built = int(pick(it, &["buildYear", "건축년도"])).filter(|y| (1900..=2100).contains(y));
    let jibun = pick(it, &["jibun", "지번"]);
    let cancel = !pick(it, &["cdealType", "해제여부"]).is_empty();
    let direct = pick(it, &["dealingGbn", "거래유형"]).contains("직거래");
    match kind {
        "apt" => {
            let amt = amount(pick(it, &["dealAmount", "거래금액"]))?;
            Some(json!([
                ym,
                opt_num(day),
                umd,
                pick(it, &["aptNm", "아파트"]),
                opt_num(area(pick(it, &["excluUseAr", "전용면적"]))),
                opt_num(floor),
                opt_num(built),
                amt,
                flag(cancel),
                flag(direct),
                jibun
            ]))
        }
        "rent" => {
            let dep = amount(pick(it, &["deposit", "보증금액", "보증금"]))?;
            let mon = amount(pick(it, &["monthlyRent", "월세금액", "월세"])).unwrap_or(0);
            let renew = pick(it, &["contractType", "계약구분"]).contains("갱신");
            Some(json!([
                ym,
                opt_num(day),
                umd,
                pick(it, &["aptNm", "아파트"]),
                opt_num(area(pick(it, &["excluUseAr", "전용면적"]))),
                opt_num(floor),
                opt_num(built),
                dep,
                mon,
                flag(renew),
                jibun
            ]))
        }
        "nrg" => {
            let amt = amount(pick(it, &["dealAmount", "거래금액"]))?;
            let share = !pick(it, &["shareDealingType", "지분거래구분"]).is_empty();
            Some(json!([
                ym,
                opt_num(day),
                umd,
                jibun,
                pick(it, &["buildingUse", "건물주용도"]),
                pick(it, &["buildingType", "유형"]),
                pick(it, &["landUse", "용도지역"]),
                opt_num(area(pick(it, &["buildingAr", "건물면적"]))),
                opt_num(area(pick(it, &["plottageAr", "대지면적"]))),
                opt_num(floor),
                opt_num(built),
                amt,
                flag(cancel),
                flag(share),
                flag(direct)
            ]))
        }
        _ => None,
    }
}

#[derive(Default)]
struct KindOut {
    rows: Vec<Value>,
    failed: Vec<String>,
    /// "ok" | "auth" | "limit" | "error"
    status: &'static str,
    note: String,
}

/// 종류별로 어느 판(상세/일반)을 쓰는지.
#[derive(Default, Clone, Copy)]
struct EndpointState {
    idx: usize,
    /// 한 번이라도 정상 응답을 받았는가
    verified: bool,
    /// 모든 판이 인증에서 막혔는가
    dead: bool,
}

fn collect_kind(cx: &mut Ctx, kind: &Kind, region: &Region, months: &[String], ep: &mut EndpointState) -> KindOut {
    let mut out = KindOut { status: "ok", ..Default::default() };
    if ep.dead {
        out.status = "auth";
        out.failed = months.to_vec();
        return out;
    }
    let mut pref = 0usize;
    let mut tags_logged = false;
    'months: for (mi, ym) in months.iter().enumerate() {
        let mut order: Vec<usize> = vec![pref];
        order.extend((0..region.lawd.len()).filter(|i| *i != pref));
        let mut got: Option<Vec<Item>> = None;
        let mut err: Option<String> = None;
        let mut oi = 0;
        while oi < order.len() {
            let li = order[oi];
            let ei = ep.idx;
            match fetch_month(cx, kind.endpoints[ei], &region.lawd[li], ym) {
                Ok(items) => {
                    ep.verified = true;
                    if !items.is_empty() {
                        pref = li;
                        got = Some(items);
                        break;
                    }
                    got.get_or_insert_with(Vec::new);
                    oi += 1;
                }
                Err(FetchErr::Auth(why)) => {
                    // 아직 확인되지 않은 판이면 다음 판으로 같은 달을 다시 시도한다
                    if !ep.verified && ei + 1 < kind.endpoints.len() {
                        ep.idx = ei + 1;
                        cx.log(&format!("  {}: {}번째 판 사용 불가({why}) → 다음 판으로 시도", kind.label, ei + 1));
                        continue;
                    }
                    ep.dead = true;
                    out.status = "auth";
                    out.note = why;
                    out.failed.extend(months[mi..].iter().cloned());
                    break 'months;
                }
                Err(FetchErr::Limit(why)) => {
                    out.status = "limit";
                    out.note = why;
                    out.failed.extend(months[mi..].iter().cloned());
                    break 'months;
                }
                Err(FetchErr::Other(why)) => {
                    err = Some(why);
                    oi += 1;
                }
            }
        }
        match got {
            Some(items) => {
                if !tags_logged && !items.is_empty() {
                    let mut tags: Vec<&str> = items[0].keys().map(String::as_str).collect();
                    tags.sort_unstable();
                    cx.log(&format!("  {} 응답 항목: {}", kind.label, tags.join(",")));
                    tags_logged = true;
                }
                let before = out.rows.len();
                out.rows.extend(items.iter().filter_map(|it| to_row(kind.id, ym, it)));
                if !items.is_empty() && out.rows.len() == before {
                    // 항목은 왔는데 한 줄도 못 만들었다 = 응답 형식이 바뀜
                    out.failed.push(ym.clone());
                    out.status = "error";
                    out.note = "응답 형식을 해석하지 못함".into();
                }
            }
            None => {
                out.failed.push(ym.clone());
                if out.status == "ok" {
                    out.status = "error";
                }
                out.note = err.unwrap_or_else(|| "받지 못함".into());
            }
        }
    }
    out
}

fn load_old(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn old_rows(old: Option<&Value>, kind: &str, keep_months: &HashSet<&str>) -> Vec<Value> {
    let Some(rows) = old.and_then(|o| o.get(kind)).and_then(|k| k.get("rows")).and_then(Value::as_array) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|r| r.get(0).and_then(Value::as_str).is_some_and(|ym| keep_months.contains(ym)))
        .cloned()
        .collect()
}

fn sort_rows(rows: &mut [Value]) {
    rows.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v.get(0).and_then(Value::as_str).unwrap_or("").to_string(),
                v.get(1).and_then(Value::as_i64).unwrap_or(0),
                v.get(2).and_then(Value::as_str).unwrap_or("").to_string(),
                v.get(3).and_then(Value::as_str).unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b)).then_with(|| a.to_string().cmp(&b.to_string()))
    });
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

fn run() -> Result<bool, String> {
    let key_raw = env::var("DATA_GO_KR_KEY").unwrap_or_default().trim().to_string();
    if key_raw.len() < 20 {
        return Err("DATA_GO_KR_KEY 가 없습니다. 저장소 Settings → Secrets and variables → Actions 에 등록하세요.".into());
    }
    let out_dir = PathBuf::from(env::args().nth(1).unwrap_or_else(|| "data/estate".into()));
    let regions = parse_regions(&env::var("RT_REGIONS").unwrap_or_else(|_| DEFAULT_REGIONS.into()))?;
    let n_months: u32 = env::var("RT_MONTHS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(24).clamp(1, 60);

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(40)))
        .http_status_as_error(false)
        .https_only(true)
        .max_redirects(0)
        .user_agent("dongne-market-collector/0.1")
        .build()
        .into();
    let mut cx = Ctx { agent, key_enc: encode_key(&key_raw), key_raw, calls: 0 };

    let (y, m, d) = kst_today();
    let months = months_back(y, m, n_months);
    let updated = format!("{y:04}-{m:02}-{d:02}");
    cx.log(&format!("기준일 {updated} · {}~{} ({}개월) · 지역 {}곳", months[0], months[months.len() - 1], months.len(), regions.len()));
    fs::create_dir_all(&out_dir).map_err(|e| format!("출력 폴더를 만들 수 없습니다: {e}"))?;

    let mut ep_state = [EndpointState::default(); 3];
    let mut wrote_any = false;
    let month_set: HashSet<&str> = months.iter().map(String::as_str).collect();

    for region in &regions {
        cx.log(&format!("[{}]", region.sgg));
        let path = out_dir.join(format!("{}.json", region.sgg));
        let old = load_old(&path);
        let mut doc = json!({ "v": 1, "sgg": region.sgg, "updated": updated, "months": months, "status": {} });
        let mut fresh_months = 0usize;

        for (ki, kind) in KINDS.iter().enumerate() {
            let res = collect_kind(&mut cx, kind, region, &months, &mut ep_state[ki]);
            let got_months = months.len() - res.failed.len();
            fresh_months += got_months;
            let mut rows = res.rows;
            let fresh = rows.len();
            // 받지 못한 달은 이전 값을 유지
            let failed: HashSet<&str> = res.failed.iter().map(String::as_str).filter(|ym| month_set.contains(ym)).collect();
            let kept = old_rows(old.as_ref(), kind.id, &failed);
            let kept_n = kept.len();
            rows.extend(kept);
            sort_rows(&mut rows);
            let status = if res.status != "ok" && kept_n > 0 && fresh == 0 { "stale" } else { res.status };
            cx.log(&format!(
                "  {}: {}건 (새로 받은 달 {}/{}{}{})",
                kind.label,
                rows.len(),
                got_months,
                months.len(),
                if kept_n > 0 { format!(", 이전 값 유지 {kept_n}건") } else { String::new() },
                if res.note.is_empty() { String::new() } else { format!(", 사유: {}", res.note) },
            ));
            if res.status != "ok" {
                let hint = match res.status {
                    "auth" => "키가 틀렸거나, 발급 직후라 아직 활성화 전이거나, 이 자료를 활용신청하지 않았습니다",
                    "limit" => "오늘 호출 한도를 넘었습니다",
                    _ => "일부 달을 받지 못했습니다",
                };
                cx.log(&format!("::warning::{} {} — {hint}", region.sgg, kind.label));
            }
            doc[kind.id] = json!({ "cols": kind.cols, "rows": rows });
            doc["status"][kind.id] = Value::from(status);
        }

        if fresh_months == 0 {
            cx.log("  새로 받은 자료가 없어 파일을 그대로 둡니다");
            continue;
        }
        let text = serde_json::to_string(&doc).map_err(|e| format!("JSON 변환 실패: {e}"))?;
        write_atomic(&path, &text).map_err(|e| format!("파일 저장 실패: {e}"))?;
        wrote_any = true;
    }
    cx.log(&format!("API 호출 {}회", cx.calls));
    Ok(wrote_any)
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => {
            println!("::error::받은 자료가 하나도 없습니다. 키와 활용신청 상태를 확인하세요.");
            process::exit(1);
        }
        Err(e) => {
            // 여기까지 오는 문구는 키를 담지 않는다(설정·파일 오류뿐).
            println!("::error::{e}");
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_774), (2026, 11, 17));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(months_back(2026, 11, 3), vec!["202609", "202610", "202611"]);
        assert_eq!(months_back(2026, 1, 3), vec!["202511", "202512", "202601"]);
        assert_eq!(months_back(2026, 11, 24).len(), 24);
        assert_eq!(months_back(2026, 11, 24)[0], "202412");
    }

    #[test]
    fn key_encoding() {
        // 디코딩 키(+,/,= 포함)는 인코딩한다
        assert_eq!(encode_key("ab+c/d=="), "ab%2Bc%2Fd%3D%3D");
        // 이미 인코딩된 키는 그대로
        assert_eq!(encode_key("ab%2Bc%2Fd%3D%3D"), "ab%2Bc%2Fd%3D%3D");
        // 16진수만으로 된 새 형식 키는 그대로
        assert_eq!(encode_key(" 0123abcdef "), "0123abcdef");
        // % 가 있어도 올바른 인코딩이 아니면 통째로 인코딩
        assert_eq!(encode_key("a%zz"), "a%25zz");
        assert_eq!(encode_key("a%2"), "a%252");
        assert_eq!(encode_key("a%2B+"), "a%252B%2B");
    }

    #[test]
    fn regions() {
        let r = parse_regions(DEFAULT_REGIONS).unwrap();
        assert_eq!(r.len(), 5);
        assert_eq!(r[0].sgg, "12270");
        assert_eq!(r[0].lawd, vec!["12270", "29155"]);
        assert!(parse_regions("12270=1227a").is_err());
        assert!(parse_regions("12270=12270&x=1").is_err());
        assert!(parse_regions("").is_err());
        assert_eq!(parse_regions("29155").unwrap()[0].lawd, vec!["29155"]);
    }

    #[test]
    fn redaction() {
        let cx = Ctx {
            agent: ureq::Agent::new_with_defaults(),
            key_enc: "se%2Bcret".into(),
            key_raw: "se+cret".into(),
            calls: 0,
        };
        assert_eq!(cx.redact("x?serviceKey=se%2Bcret&y"), "x?serviceKey=***&y");
        assert_eq!(cx.redact("raw se+cret here"), "raw *** here");
    }

    fn item(pairs: &[(&str, &str)]) -> Item {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn rows() {
        let apt = item(&[
            ("aptNm", "진월아파트"), ("umdNm", "진월동"), ("dealAmount", "38,500"), ("dealDay", "7"),
            ("excluUseAr", "84.98"), ("floor", "12"), ("buildYear", "2004"), ("dealingGbn", "직거래"),
            ("cdealType", "O"), ("jibun", "300"),
        ]);
        assert_eq!(
            to_row("apt", "202610", &apt).unwrap(),
            json!(["202610", 7, "진월동", "진월아파트", 84.98, 12, 2004, 38500, 1, 1, "300"])
        );
        let rent = item(&[("aptNm", "한신"), ("umdNm", "주월동"), ("deposit", "20,000"), ("monthlyRent", "0"), ("contractType", "갱신"), ("excluUseAr", "59.9")]);
        assert_eq!(
            to_row("rent", "202609", &rent).unwrap(),
            json!(["202609", null, "주월동", "한신", 59.9, null, null, 20000, 0, 1, ""])
        );
        let nrg = item(&[
            ("umdNm", "봉선동"), ("jibun", "1**"), ("buildingUse", "제2종근린생활"), ("buildingType", "일반"),
            ("landUse", "제2종일반주거"), ("buildingAr", "331.2"), ("plottageAr", "198"), ("dealAmount", "150,000"), ("dealDay", "3"),
        ]);
        assert_eq!(
            to_row("nrg", "202608", &nrg).unwrap(),
            json!(["202608", 3, "봉선동", "1**", "제2종근린생활", "일반", "제2종일반주거", 331.2, 198.0, null, null, 150000, 0, 0, 0])
        );
        // 금액이 없으면 버린다
        assert!(to_row("apt", "202610", &item(&[("aptNm", "x")])).is_none());
        // 한글 태그
        let ko = item(&[("아파트", "한신"), ("법정동", "진월동"), ("거래금액", "31,000"), ("일", "15"), ("전용면적", "59.99"), ("층", "5"), ("건축년도", "1999")]);
        assert_eq!(
            to_row("apt", "202503", &ko).unwrap(),
            json!(["202503", 15, "진월동", "한신", 59.99, 5, 1999, 31000, 0, 0, ""])
        );
    }

    #[test]
    fn old_rows_kept_only_for_failed_months() {
        let old = json!({ "apt": { "rows": [["202609", 1, "a", "b"], ["202610", 2, "a", "b"], ["202301", 3, "a", "b"]] } });
        let keep: HashSet<&str> = ["202610"].into_iter().collect();
        let got = old_rows(Some(&old), "apt", &keep);
        assert_eq!(got, vec![json!(["202610", 2, "a", "b"])]);
        assert!(old_rows(None, "apt", &keep).is_empty());
        assert!(old_rows(Some(&old), "rent", &keep).is_empty());
    }
}
