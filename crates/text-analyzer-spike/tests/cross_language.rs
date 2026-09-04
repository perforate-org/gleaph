//! Cross-language behavior fixtures (plan 0330/0331 follow-up): what units each
//! candidate emits for Chinese and Korean samples, documenting the language
//! coverage of the analyzer set. These are OBSERVATIONAL fixtures — they record
//! the measured behavior rather than asserting a target contract.

#[test]
fn chinese_and_korean_samples_through_candidates() {
    let samples = [
        // Chinese: 你好世界 (hello world) + a longer sentence
        ("chinese_short", "你好世界"),
        ("chinese_long", "知识图谱数据库在人工智能中的应用"),
        // Korean: school+particle (agglutination) + a sentence
        ("korean_short", "학교에서"),
        ("korean_long", "지식 그래프 데이터베이스는 인공지능의 응용이다"),
        // Japanese control: the documented recall pair from plan 0330
        ("japanese_inflected", "走った"),
    ];
    let candidates = text_analyzer_spike::harness::candidates();
    for (name, text) in samples {
        for (candidate, analyze) in &candidates {
            let units = analyze(text);
            println!("[{candidate}/{name}] {text:?} -> {units:?}");
        }
    }
    // Japanese control assertion (documented recall contract)
    let vibrato = candidates
        .iter()
        .find(|(name, _)| *name == "vibrato")
        .expect("vibrato feature enabled");
    assert!(
        (vibrato.1)("走った")
            .iter()
            .any(|u| u == "走る"),
        "vibrato lemma recall on 走った must yield 走る"
    );
}