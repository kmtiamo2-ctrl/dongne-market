//! 실거래가 API 응답(XML) 해석.
//!
//! 응답 태그 이름은 시기에 따라 영문(aptNm, dealAmount …)과 한글(아파트, 거래금액 …)
//! 두 가지가 쓰였으므로, 항목을 "태그 이름 → 값" 표로 읽은 뒤 후보 이름으로 찾는다.

use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;

pub type Item = HashMap<String, String>;

#[derive(Debug, Default)]
pub struct Page {
    /// 결과 코드. 정상은 "00" 또는 "000".
    pub code: String,
    pub msg: String,
    pub total: Option<u64>,
    pub items: Vec<Item>,
}

impl Page {
    pub fn ok(&self) -> bool {
        matches!(self.code.as_str(), "00" | "000" | "0")
    }
    /// 자료 없음(정상 취급)
    pub fn no_data(&self) -> bool {
        self.code == "03" || self.code == "3"
    }
}

/// 응답 한 장을 해석한다. XML이 아니면 Err.
pub fn parse_page(xml: &str) -> Result<Page, String> {
    let mut reader = Reader::from_str(xml);
    let mut page = Page::default();
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut item: Option<Item> = None;
    let mut saw_root = false;

    loop {
        match reader.read_event() {
            Err(e) => return Err(format!("XML 해석 실패: {e}")),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                saw_root = true;
                let name = e.name().as_ref().to_string();
                if name == "item" && item.is_none() {
                    item = Some(Item::new());
                }
                path.push(name);
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                saw_root = true;
                let name = e.name().as_ref().to_string();
                if let Some(it) = item.as_mut() {
                    if name != "item" {
                        it.insert(name, String::new());
                    }
                }
            }
            Ok(Event::Text(t)) => {
                let s: &str = t.as_ref();
                text.push_str(s);
            }
            Ok(Event::CData(t)) => {
                let s: &str = t.as_ref();
                text.push_str(s);
            }
            Ok(Event::GeneralRef(r)) => {
                // &amp; &#39; 같은 참조
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    text.push(c);
                } else {
                    let name: &str = r.as_ref();
                    match name {
                        "amp" => text.push('&'),
                        "lt" => text.push('<'),
                        "gt" => text.push('>'),
                        "quot" => text.push('"'),
                        "apos" => text.push('\''),
                        _ => {} // 알 수 없는 참조는 버린다(외부 엔티티를 풀지 않는다)
                    }
                }
            }
            Ok(Event::End(_)) => {
                let name = path.pop().unwrap_or_default();
                let value = text.trim().to_string();
                text.clear();
                if name == "item" {
                    if let Some(it) = item.take() {
                        page.items.push(it);
                    }
                } else if let Some(it) = item.as_mut() {
                    // item 바로 아래 값만 받는다
                    if path.last().map(String::as_str) == Some("item") {
                        it.insert(name, value);
                    }
                } else {
                    match name.as_str() {
                        "resultCode" | "returnReasonCode" => {
                            if page.code.is_empty() {
                                page.code = value;
                            }
                        }
                        "resultMsg" | "returnAuthMsg" => {
                            if page.msg.is_empty() || name == "returnAuthMsg" {
                                page.msg = value;
                            }
                        }
                        "errMsg" => {
                            if page.msg.is_empty() {
                                page.msg = value;
                            }
                        }
                        "totalCount" => page.total = value.parse().ok(),
                        _ => {}
                    }
                }
            }
            Ok(_) => {}
        }
    }
    if !saw_root {
        return Err("XML 응답이 아닙니다".into());
    }
    Ok(page)
}

/// 후보 이름 중 처음으로 값이 있는 것을 돌려준다.
pub fn pick<'a>(it: &'a Item, names: &[&str]) -> &'a str {
    for n in names {
        if let Some(v) = it.get(*n) {
            let v = v.trim();
            if !v.is_empty() {
                return v;
            }
        }
    }
    ""
}

/// "120,000" → 120000 (만원). 숫자가 아니면 None.
pub fn amount(s: &str) -> Option<u64> {
    let t: String = s.chars().filter(|c| !c.is_whitespace() && *c != ',').collect();
    if t.is_empty() {
        return None;
    }
    t.parse().ok()
}

/// 면적: 소수 둘째 자리까지.
pub fn area(s: &str) -> Option<f64> {
    let t: String = s.chars().filter(|c| !c.is_whitespace() && *c != ',').collect();
    let v: f64 = t.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v * 100.0).round() / 100.0)
}

pub fn int(s: &str) -> Option<i64> {
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<response><header><resultCode>000</resultCode><resultMsg>OK</resultMsg></header>
<body><items>
<item><aptDong> </aptDong><aptNm>진월 A&amp;B 아파트</aptNm><aptSeq>29155-123</aptSeq><buildYear>2004</buildYear>
<cdealDay> </cdealDay><cdealType> </cdealType><dealAmount>   38,500</dealAmount><dealDay>7</dealDay><dealMonth>10</dealMonth>
<dealYear>2026</dealYear><dealingGbn>중개거래</dealingGbn><excluUseAr>84.9812</excluUseAr><floor>12</floor><jibun>300</jibun>
<sggCd>29155</sggCd><umdNm>진월동</umdNm></item>
<item><aptNm>해제된집</aptNm><cdealDay>26.10.20</cdealDay><cdealType>O</cdealType><dealAmount>20,000</dealAmount>
<dealDay>1</dealDay><dealMonth>10</dealMonth><dealYear>2026</dealYear><excluUseAr>59.5</excluUseAr><floor>-1</floor><umdNm>주월동</umdNm><rgstDate/></item>
</items><numOfRows>1000</numOfRows><pageNo>1</pageNo><totalCount>2</totalCount></body></response>"#;

    #[test]
    fn english_tags() {
        let p = parse_page(EN).unwrap();
        assert!(p.ok());
        assert_eq!(p.total, Some(2));
        assert_eq!(p.items.len(), 2);
        let a = &p.items[0];
        assert_eq!(pick(a, &["aptNm", "아파트"]), "진월 A&B 아파트");
        assert_eq!(amount(pick(a, &["dealAmount", "거래금액"])), Some(38500));
        assert_eq!(area(pick(a, &["excluUseAr"])), Some(84.98));
        assert_eq!(pick(a, &["cdealType"]), "");
        let b = &p.items[1];
        assert_eq!(pick(b, &["cdealType"]), "O");
        assert_eq!(int(pick(b, &["floor"])), Some(-1));
        assert_eq!(pick(b, &["rgstDate"]), "");
    }

    #[test]
    fn korean_tags() {
        let xml = "<response><header><resultCode>00</resultCode><resultMsg>NORMAL SERVICE.</resultMsg></header><body><items>\
<item><거래금액> 31,000</거래금액><건축년도>1999</건축년도><년>2025</년><법정동> 진월동</법정동><아파트>한신</아파트><월>3</월><일>15</일><전용면적>59.99</전용면적><층>5</층></item>\
</items><totalCount>1</totalCount></body></response>";
        let p = parse_page(xml).unwrap();
        assert!(p.ok());
        let a = &p.items[0];
        assert_eq!(pick(a, &["aptNm", "아파트"]), "한신");
        assert_eq!(pick(a, &["umdNm", "법정동"]), "진월동");
        assert_eq!(amount(pick(a, &["dealAmount", "거래금액"])), Some(31000));
    }

    #[test]
    fn gateway_error() {
        let xml = "<OpenAPI_ServiceResponse><cmmMsgHeader><errMsg>SERVICE ERROR</errMsg>\
<returnAuthMsg>SERVICE_KEY_IS_NOT_REGISTERED_ERROR</returnAuthMsg><returnReasonCode>30</returnReasonCode></cmmMsgHeader></OpenAPI_ServiceResponse>";
        let p = parse_page(xml).unwrap();
        assert!(!p.ok());
        assert_eq!(p.code, "30");
        assert_eq!(p.msg, "SERVICE_KEY_IS_NOT_REGISTERED_ERROR");
        assert!(p.items.is_empty());
    }

    #[test]
    fn not_xml() {
        assert!(parse_page("Unauthorized").is_err());
        assert!(parse_page("").is_err());
    }

    #[test]
    fn numbers() {
        assert_eq!(amount("1,234,567"), Some(1234567));
        assert_eq!(amount(""), None);
        assert_eq!(amount("abc"), None);
        assert_eq!(area("-3"), None);
        assert_eq!(area("123.456"), Some(123.46));
    }
}
