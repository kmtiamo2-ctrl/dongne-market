# 우리동네 상권분석

전남·광주 상가(상권)정보(소상공인시장진흥공단, 2026.06)로 만든 상권 지도·분석 도구.
GitHub Pages로 호스팅되는 정적 사이트이며 서버·비용 없이 동작합니다.

- `index.html` — 사이트 전체 (지도·분석·창업 아이템·관심 상가)
- `config.js` — 카카오맵 JavaScript 키
- `data/` — 시군구별 상가 데이터(JSON)

## 부동산 실거래가 자동 수집

- `collector/` — 국토교통부 실거래가 공개 API(아파트 매매·전월세, 상업업무용 매매)를 모으는 Rust 프로그램
- `.github/workflows/estate.yml` — 매월 4일 새벽에 수집기를 돌려 `data/estate/<시군구코드>.json` 을 갱신
- 인증키는 저장소 Secrets 의 `DATA_GO_KR_KEY` 에만 둔다. 코드·설정 파일·로그에는 넣지 않는다.
