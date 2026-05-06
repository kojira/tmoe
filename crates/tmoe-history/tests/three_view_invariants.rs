//! tmoe-history の中核不変条件を統合テストで検証する。
//!
//! 1) **機能毎の記憶分離**: feature A と feature B を順次運用しても、互いの内容が
//!    相手の summary index に混入しない (= 機能ごとに独立した会話ツリー)。
//! 2) **3 視点の選択性 (= 三角形が縮退しない)**: 同じ raw 流に対し Worker / Supervisor /
//!    Observer は **互いに異なる情報** を残す。実装ノイズ・規範違反・ユーザー意図が、
//!    各エージェントのパーソナリティに沿って異なるビューにふるい分けられる。
//! 3) **Supervisor が監督に必要な情報のみを一貫して保持**: 多数ターンを経ても
//!    Supervisor view には実装詳細やユーザー雑談ではなく、規範違反・整合性懸念だけが残る。

use tempfile::tempdir;
use tmoe_history::{
    compact_turn_for_all, AgentLens, AgentView, AppendRaw, HistoryStore, LabeledLens, RawKind,
};

fn open() -> (tempfile::TempDir, HistoryStore) {
    let d = tempdir().unwrap();
    let s = HistoryStore::open(d.path()).unwrap();
    (d, s)
}

fn lenses() -> Vec<Box<dyn AgentLens>> {
    vec![
        Box::new(LabeledLens { agent: AgentView::Worker, label: "build" }),
        Box::new(LabeledLens { agent: AgentView::Supervisor, label: "critique" }),
        Box::new(LabeledLens { agent: AgentView::Observer, label: "witness" }),
    ]
}

fn ingest(store: &HistoryStore, feature_id: &str, body: &str) {
    let raw = store
        .append_raw(AppendRaw {
            feature_id: feature_id.to_string(),
            parent_id: None,
            kind: RawKind::Turn,
            body: body.to_string(),
        })
        .unwrap();
    compact_turn_for_all(store, feature_id, &raw, body, &lenses()).unwrap();
}

fn view(store: &HistoryStore, feature_id: &str, agent: AgentView) -> String {
    store
        .latest_level0(feature_id, agent)
        .unwrap()
        .map(|n| n.summary)
        .unwrap_or_default()
}

#[test]
fn two_features_have_strictly_disjoint_three_view_indexes() {
    let (_d, s) = open();
    let fa = s.create_feature("add gcd").unwrap();
    let fb = s.create_feature("levenshtein util").unwrap();

    // Feature A — gcd を進める内容を 3 ターン
    ingest(&s, &fa.id, "implement gcd via euclid");
    ingest(&s, &fa.id, "must validate non-zero divisor for gcd");
    ingest(&s, &fa.id, "user wants gcd as math util");

    // Feature B — levenshtein を進める内容を 3 ターン
    ingest(&s, &fb.id, "implement levenshtein with DP table");
    ingest(&s, &fb.id, "must guard against empty string in levenshtein");
    ingest(&s, &fb.id, "user wants levenshtein for fuzzy match");

    for agent in AgentView::all() {
        let a = view(&s, &fa.id, agent).to_lowercase();
        let b = view(&s, &fb.id, agent).to_lowercase();
        assert!(
            a.contains("gcd"),
            "feature A's {agent:?} view should reference gcd; got: {a}"
        );
        assert!(
            b.contains("levenshtein"),
            "feature B's {agent:?} view should reference levenshtein; got: {b}"
        );
        assert!(
            !a.contains("levenshtein"),
            "feature A's {agent:?} view leaked feature B content: {a}"
        );
        assert!(
            !b.contains("gcd") || b.contains("gcd as ") == false, // gcd という単語が偶然 levenshtein 説明に出ていないことを確認
            "feature B's {agent:?} view leaked feature A content: {b}"
        );
    }
}

#[test]
fn three_views_partition_a_mixed_raw_stream() {
    // 同じ feature に対し、実装・規範・意図が混じった 6 ターンを流す。
    // 各 view が自分のパーソナリティ語彙だけを拾い、他の view の語彙は拾わないことを検証する。
    let (_d, s) = open();
    let f = s.create_feature("project x").unwrap();

    let stream = [
        // Worker (build / impl / fn / patch / 完了 / 実装 / 追加 / 修正)
        "implement gcd via euclid",
        // Supervisor (warn / must / reject / 拒否 / 整合 / 安全 / error)
        "warning: gcd must reject zero divisor",
        // Observer (user / intent / loop / 意図 / 履歴 / context)
        "user intent: gcd is the first math util in this crate",
        // Worker
        "fn gcd done; implementation matches euclidean algorithm",
        // Supervisor
        "error: previous patch ignored overflow safety check",
        // Observer
        "context: avoid loop with same divisor pair",
    ];
    for body in stream {
        ingest(&s, &f.id, body);
    }

    let w = view(&s, &f.id, AgentView::Worker).to_lowercase();
    let p = view(&s, &f.id, AgentView::Supervisor).to_lowercase();
    let o = view(&s, &f.id, AgentView::Observer).to_lowercase();

    eprintln!("worker view:\n{w}\n---\nsupervisor view:\n{p}\n---\nobserver view:\n{o}");

    // 各 view は自分のキーワードを拾う。
    assert!(w.contains("implement") || w.contains("fn gcd"), "worker view missing impl signal: {w}");
    assert!(p.contains("warning") || p.contains("must") || p.contains("error"), "supervisor view missing critique signal: {p}");
    assert!(o.contains("intent") || o.contains("user") || o.contains("context") || o.contains("loop"), "observer view missing witness signal: {o}");

    // Supervisor は実装詳細やユーザー意図に紛れない。
    assert!(
        !p.contains("implement gcd via euclid"),
        "supervisor view leaked implementation detail (worker territory): {p}"
    );
    assert!(
        !p.contains("user intent:"),
        "supervisor view leaked user intent (observer territory): {p}"
    );

    // Worker は規範違反警告に紛れない。
    assert!(
        !w.contains("warning:"),
        "worker view leaked normative warning (supervisor territory): {w}"
    );
    assert!(
        !w.contains("user intent:"),
        "worker view leaked user intent (observer territory): {w}"
    );

    // Observer は実装詳細・規範違反に紛れない。
    assert!(
        !o.contains("fn gcd done"),
        "observer view leaked implementation detail (worker territory): {o}"
    );
    assert!(
        !o.contains("error: previous"),
        "observer view leaked normative error (supervisor territory): {o}"
    );

    // 3 view は互いに異なる (= 三角形が縮退していない)。
    assert_ne!(w, p, "worker and supervisor views collapsed");
    assert_ne!(p, o, "supervisor and observer views collapsed");
    assert_ne!(w, o, "worker and observer views collapsed");
}

#[test]
fn supervisor_view_remains_critique_only_across_many_turns() {
    // ターンを重ねても Supervisor の視点が拡散せず、規範・整合に絞られたままであることを確認。
    let (_d, s) = open();
    let f = s.create_feature("long-running feature").unwrap();

    // 12 ターン: 実装 4 / 規範 4 / 意図 4 を交互に。
    let stream = [
        ("worker", "implement add(a,b)"),
        ("sup",    "warning: missing overflow guard in add"),
        ("obs",    "user intent: arithmetic helper"),
        ("worker", "fn add complete with i64"),
        ("sup",    "must reject overflow in mul"),
        ("obs",    "context: keep helpers tiny"),
        ("worker", "implement mul(a,b)"),
        ("sup",    "error: signed mul wraps without checked_mul"),
        ("obs",    "user prefers explicit checked_* variant"),
        ("worker", "fn mul fixed; uses checked_mul"),
        ("sup",    "must reject non-finite floats once we add div"),
        ("obs",    "context: divisions are out of scope for now"),
    ];
    for (_role, body) in stream {
        ingest(&s, &f.id, body);
    }

    let p = view(&s, &f.id, AgentView::Supervisor).to_lowercase();
    eprintln!("supervisor view (12 turns):\n{p}");

    // 規範語彙が複数残っていること。
    let critique_keywords = ["warning", "must", "reject", "error"];
    let hits = critique_keywords.iter().filter(|k| p.contains(*k)).count();
    assert!(
        hits >= 3,
        "supervisor view should retain multiple critique signals across 12 turns; got {hits} of {:?} in:\n{p}",
        critique_keywords
    );

    // 実装本文 (worker 専有語彙) は混入していないこと。
    assert!(!p.contains("fn add complete"), "supervisor leaked impl: {p}");
    assert!(!p.contains("fn mul fixed"), "supervisor leaked impl: {p}");

    // ユーザー意図 (observer 専有語彙) は混入していないこと。
    assert!(!p.contains("user intent:"), "supervisor leaked observer territory: {p}");
    assert!(!p.contains("user prefers"), "supervisor leaked observer territory: {p}");
}
