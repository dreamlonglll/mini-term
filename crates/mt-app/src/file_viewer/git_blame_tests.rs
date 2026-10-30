use std::path::PathBuf;

use chrono::FixedOffset;
use mt_project::git::{BlameCommit, HeadSignature};

use super::*;

fn commit(hash_digit: char, author: &str, time: i64) -> BlameCommit {
    BlameCommit {
        hash: hash_digit.to_string().repeat(40),
        author: author.into(),
        author_mail: format!("{author}@example.com"),
        author_time: time,
        author_tz: "+0800".into(),
        summary: format!("by {author}"),
    }
}

/// 三个提交:0 = 旧提交、1 = 未提交、2 = 新提交。行 → 提交:0 0 1 2 2 0
fn sample() -> BlameResult {
    BlameResult {
        commits: vec![
            commit('a', "alice", 1_000),
            commit('0', "Not Committed Yet", 9_000),
            commit('b', "张三", 5_000),
        ],
        lines: vec![0, 0, 1, 2, 2, 0],
    }
}

#[test]
fn 列宽_宽字符两列_截断带省略号() {
    assert_eq!(text_cols("alice"), 5);
    assert_eq!(text_cols("张三"), 4);
    assert_eq!(text_cols("ａ"), 2, "全角拉丁也是宽字符");
    assert_eq!(truncate_cols("alice", 12), "alice");
    assert_eq!(truncate_cols("abcdefghijklmnop", 12), "abcdefghijk…");
    assert_eq!(text_cols(&truncate_cols("abcdefghijklmnop", 12)), 12);
    // 截断后不超列宽,宁可少一个字也不压线
    let cut = truncate_cols("张三李四王五赵六", 12);
    assert_eq!(cut, "张三李四王…");
    assert!(text_cols(&cut) <= 12);
}

#[test]
fn 短日期_今年只写月日_往年带年份_按时区换算() {
    let tz = FixedOffset::east_opt(8 * 3600).unwrap();
    let at = |y, m, d, h| tz.with_ymd_and_hms(y, m, d, h, 30, 0).unwrap().timestamp();
    let now = at(2026, 10, 29, 12);
    assert_eq!(short_date(at(2026, 10, 21, 9), now, &tz), "10-21");
    assert_eq!(short_date(at(2024, 3, 5, 9), now, &tz), "2024-03-05");
    // UTC 是 2025 年最后一晚,东八区已经是 2026 年元旦:按本地时区算「今年」
    let utc_new_year_eve = chrono::Utc
        .with_ymd_and_hms(2025, 12, 31, 20, 0, 0)
        .unwrap()
        .timestamp();
    assert_eq!(short_date(utc_new_year_eve, now, &tz), "01-01");
    assert_eq!(full_time(at(2026, 10, 21, 9), &tz), "2026-10-21 09:30");
}

#[test]
fn 新旧分档_按排名_最新最亮() {
    assert_eq!(age_tiers(&[100, 200, 300]), vec![0, 2, 4]);
    assert_eq!(
        age_tiers(&[10, 30, 20, 30]),
        vec![0, 4, 2, 4],
        "同一时刻同一档"
    );
    assert_eq!(age_tiers(&[5, 5]), vec![4, 4], "只有一个时刻就是最新");
    assert!(age_tiers(&[]).is_empty());
    // 一个特别老的初始提交不会把其余的全挤进最亮一档
    let tiers = age_tiers(&[0, 1_000_000, 1_000_001, 1_000_002, 1_000_003]);
    assert_eq!(tiers, vec![0, 1, 2, 3, 4]);
    for pair in age_tiers(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).windows(2) {
        assert!(pair[0] <= pair[1], "档位随新旧单调");
    }
}

#[test]
fn 行归属_直通_映射_未提交() {
    let result = sample();
    // 缓冲区没变:行号直通
    assert_eq!(line_blame(&result, None, 0), Some(LineBlame::Commit(0)));
    assert_eq!(
        line_blame(&result, None, 2),
        Some(LineBlame::Uncommitted),
        "全零哈希"
    );
    assert_eq!(line_blame(&result, None, 3), Some(LineBlame::Commit(2)));
    assert_eq!(line_blame(&result, None, 6), None, "末尾空行不画");

    // 开头插了一行、又改了原来的第 3 行:当前行 → blame 时的行
    let map = vec![None, Some(0), Some(1), Some(2), None, Some(4), Some(5)];
    let map = Some(map.as_slice());
    assert_eq!(
        line_blame(&result, map, 0),
        Some(LineBlame::Uncommitted),
        "新插的行"
    );
    assert_eq!(line_blame(&result, map, 1), Some(LineBlame::Commit(0)));
    assert_eq!(
        line_blame(&result, map, 4),
        Some(LineBlame::Uncommitted),
        "改过的行"
    );
    assert_eq!(line_blame(&result, map, 5), Some(LineBlame::Commit(2)));
    assert_eq!(line_blame(&result, map, 7), None);
}

#[test]
fn 连续段的首行() {
    let result = sample();
    assert_eq!(run_start(&result, None, 1), 0);
    assert_eq!(run_start(&result, None, 2), 2);
    assert_eq!(run_start(&result, None, 4), 3);
    assert_eq!(run_start(&result, None, 5), 5);
    assert_eq!(run_start(&result, None, 0), 0);
}

#[test]
fn 显示件_作者截断_未提交不占作者列宽() {
    let mut result = sample();
    result.commits[2].author = "一个非常非常长的作者名字".into();
    let repo = RepoFile {
        root: PathBuf::from("/repo"),
        rel_path: "a.rs".into(),
        signature: HeadSignature::default(),
    };
    let view = BlameView::new(repo, result, "x\n".into(), 7);
    assert_eq!(view.generation, 7);
    assert!(view.labels[2].author.ends_with('…'));
    assert_eq!(view.author_cols, text_cols(&view.labels[2].author));
    assert!(view.author_cols <= AUTHOR_MAX_COLS);
    // 日期栏:今年 5 列 / 往年 10 列(这里的时间戳在 1970 年)
    assert_eq!(view.date_cols, 10);
    // 档位只在已提交的提交之间排:最新的那个最亮,未提交那条(时间是「现在」)
    // 不把它挤下去
    assert_eq!(view.labels[2].alpha, TIER_ALPHA[4]);
    assert_eq!(view.labels[0].alpha, TIER_ALPHA[0]);
}

#[test]
fn 行映射_没变直通_变了按对齐_旧代结果丢弃() {
    let input = BlameDiffInput {
        base: "a\nb\n".into(),
        generation: 3,
    };
    assert!(input.compute("a\nb\n").map.is_none());
    let mapped = input.compute("x\na\nb\n");
    assert_eq!(mapped.map, Some(vec![None, Some(0), Some(1)]));

    let repo = RepoFile {
        root: PathBuf::from("/repo"),
        rel_path: "a.rs".into(),
        signature: HeadSignature::default(),
    };
    let mut annotate = Annotations {
        enabled: true,
        view: Some(Rc::new(BlameView::new(repo, sample(), "a\nb\n".into(), 3))),
        ..Annotations::default()
    };
    // 同代:收下
    annotate.apply_line_map(input.compute("x\na\nb\n"));
    assert!(annotate.map.is_some());
    // 旧代(blame 已经重跑过):丢弃
    let stale = BlameDiffInput {
        base: "a\nb\n".into(),
        generation: 2,
    };
    annotate.map = None;
    annotate.apply_line_map(stale.compute("x\na\nb\n"));
    assert!(annotate.map.is_none());
    // 关着注解时不算映射
    annotate.enabled = false;
    assert!(annotate.diff_input().is_none());
}

/// 「注解」开关的提示随开关状态取文案(开着说隐藏、关着说显示),两个 key 中英齐全。
#[test]
fn 注解开关提示随状态() {
    assert_eq!(annotate_tip_key(true), "hideAnnotations");
    assert_eq!(annotate_tip_key(false), "showAnnotations");
    for on in [true, false] {
        for locale in [mt_i18n::Locale::Zh, mt_i18n::Locale::En] {
            assert!(mt_i18n::lookup(locale, "gitGutter", annotate_tip_key(on)).is_some());
        }
    }
}
