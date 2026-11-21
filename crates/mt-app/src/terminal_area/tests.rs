use super::tab_menu::{TabMenuAction, maximize_menu_state, tab_menu_actions};
use super::*;

/// 焦点落空:终端页上没有弹窗压着才接;有接手的终端就给它,一个不剩落到兜底上。
#[test]
fn 焦点落空交给活动终端否则落到兜底() {
    let pane = || Some(("pane-a".to_string(), true));
    assert_eq!(
        lost_focus_handoff(true, false, pane()),
        FocusHandoff::Pane("pane-a".into())
    );
    assert_eq!(
        lost_focus_handoff(true, false, None),
        FocusHandoff::Sink,
        "项目里一个终端都不剩:落到兜底,不能落空"
    );
    assert_eq!(
        lost_focus_handoff(true, false, Some(("pane-a".into(), false))),
        FocusHandoff::Sink,
        "接手的 pane 还没有终端实体:交不出去,也落到兜底"
    );
    assert_eq!(
        lost_focus_handoff(false, false, pane()),
        FocusHandoff::Leave,
        "文件页自己管焦点"
    );
    assert_eq!(
        lost_focus_handoff(true, true, pane()),
        FocusHandoff::Leave,
        "弹窗 / 菜单压着:不能把键盘抢到底下的终端上"
    );
}

#[test]
fn 切过来那一下凑成的双击不改名() {
    let mut guard = SwitchClickGuard::default();
    // 点未激活的 b:第一下切过去,紧跟的第二、三下都不改名
    assert_eq!(guard.click("b", false, 1), SwitchClick::Switch);
    assert_eq!(guard.click("b", true, 2), SwitchClick::Ignore);
    assert_eq!(guard.click("b", true, 3), SwitchClick::Ignore);
    // 过了双击时窗再双击已激活的 b:改名
    assert_eq!(guard.click("b", true, 1), SwitchClick::Switch);
    assert_eq!(guard.click("b", true, 2), SwitchClick::Rename);
    // 切到 c 之后双击 b(先切回去)仍不改名;双击 c 照常改名
    assert_eq!(guard.click("c", false, 1), SwitchClick::Switch);
    assert_eq!(guard.click("b", false, 1), SwitchClick::Switch);
    assert_eq!(guard.click("b", true, 2), SwitchClick::Ignore);
    assert_eq!(guard.click("c", false, 1), SwitchClick::Switch);
    assert_eq!(guard.click("c", true, 1), SwitchClick::Switch);
    assert_eq!(guard.click("c", true, 2), SwitchClick::Rename);
}

/// 存的百分比正常时按它切,和为 1。
#[test]
fn 比例按存的百分比换算() {
    let f = split_fractions(&[30.0, 70.0], 2);
    assert!((f[0] - 0.3).abs() < 1e-9 && (f[1] - 0.7).abs() < 1e-9);
    // 和不是 100 的老数据(拖动写回时有浮点误差)照样归一
    let f = split_fractions(&[1.0, 1.0, 2.0], 3);
    assert_eq!(f, vec![0.25, 0.25, 0.5]);
}

/// 布局快照的失效判据(见 [`sync_layout_snapshot`])。
///
/// 三条一起看住这道闸:① 内容一变必须换新快照(否则画的是上一帧的分屏树);
/// ② 内容没变不许换(换了就等于把「每帧深拷贝」原样搬了回来);
/// ③ 活树没了要清空,且「本来就是空的」不算换过 —— `split_states` 的回收
/// 挂在这个返回值上,恒真会把每帧一次的全项目遍历又请回来。
#[test]
fn 布局快照按内容失效() {
    use crate::tree::PaneState;

    let mut snapshot: Option<std::rc::Rc<SplitNode>> = None;
    let mut live = SplitNode::leaf(PaneState::new("pwsh"));

    // 第一次:从无到有
    assert!(sync_layout_snapshot(&mut snapshot, Some(&live)));
    assert_eq!(snapshot.as_deref(), Some(&live));

    // 同一棵树再来一次:不换,也不重新拷
    let kept = snapshot.clone().unwrap();
    assert!(!sync_layout_snapshot(&mut snapshot, Some(&live)));
    assert!(
        std::rc::Rc::ptr_eq(&kept, snapshot.as_ref().unwrap()),
        "内容没变却重拷了一棵树"
    );

    // 就地改一个字段(store 那边就是这么改的,指针不变)→ 必须换
    if let SplitNode::Leaf { panes, .. } = &mut live {
        panes[0].status = crate::tree::PaneStatus::AiWorking;
    }
    assert!(sync_layout_snapshot(&mut snapshot, Some(&live)));
    assert_eq!(snapshot.as_deref(), Some(&live));

    // 活树没了 → 清空并报「换过」(这一帧要顺带回收 split_states)
    assert!(sync_layout_snapshot(&mut snapshot, None));
    assert!(snapshot.is_none());
    // 已经是空的了 → 不算换过
    assert!(!sync_layout_snapshot(&mut snapshot, None));
}

/// 分屏进场滑入的端点严丝合缝:起点满偏移、终点精确归零,越界进度一律钳住
/// (残留半像素会让终端内容永久错位半格)。
#[test]
fn 换场滑入端点严丝合缝() {
    assert_eq!(swap_slide_offset(0.0), SWAP_SLIDE_PX);
    assert_eq!(swap_slide_offset(1.0), 0.0);
    assert_eq!(swap_slide_offset(1.5), 0.0, "越界进度不许滑过头再回弹");
    assert_eq!(swap_slide_offset(-0.5), SWAP_SLIDE_PX);
    assert!((swap_slide_offset(0.5) - SWAP_SLIDE_PX * 0.5).abs() < 1e-6);
}

/// 还原动画的目标矩形按分屏比例换算:嵌套 split 里也要算对,
/// 端点(原点/尺寸)与 render_node 的铺法一致。
#[test]
fn 还原目标矩形按分屏比例换算() {
    use crate::tree::{PaneState, gen_id};
    let leaf = |name: &str| SplitNode::leaf(PaneState::new(name));
    // [a(30%) | [b(50%) / c(50%)](70%)],区域 1000×800
    let (a, b, c) = (leaf("a"), leaf("b"), leaf("c"));
    let (aid, bid, cid) = (a.id().to_string(), b.id().to_string(), c.id().to_string());
    let inner = SplitNode::Split {
        id: gen_id("split"),
        direction: SplitDirection::Vertical,
        sizes: vec![50.0, 50.0],
        children: vec![b, c],
    };
    let root = SplitNode::Split {
        id: gen_id("split"),
        direction: SplitDirection::Horizontal,
        sizes: vec![30.0, 70.0],
        children: vec![a, inner],
    };

    let ra = leaf_rect_in(&root, &aid, 0.0, 0.0, 1000.0, 800.0).unwrap();
    assert_eq!(
        (ra.left, ra.top, ra.width, ra.height),
        (0.0, 0.0, 300.0, 800.0)
    );
    let rb = leaf_rect_in(&root, &bid, 0.0, 0.0, 1000.0, 800.0).unwrap();
    assert_eq!(
        (rb.left, rb.top, rb.width, rb.height),
        (300.0, 0.0, 700.0, 400.0)
    );
    let rc = leaf_rect_in(&root, &cid, 0.0, 0.0, 1000.0, 800.0).unwrap();
    assert_eq!(
        (rc.left, rc.top, rc.width, rc.height),
        (300.0, 400.0, 700.0, 400.0)
    );
    assert!(leaf_rect_in(&root, "leaf-不存在", 0.0, 0.0, 1000.0, 800.0).is_none());
}

/// push 两层首尾相接:往右切(dir=+1)时,进场层从整幅在右(+1)推到 0、
/// 出场层从 0 推到整幅在左(-1);任一时刻两层相距恒为一幅宽,中间不裂缝
/// 不重叠。往左切整体镜像。终点必须精确归零/归幅。
#[test]
fn push两层首尾相接且端点精确() {
    for dir in [1.0f32, -1.0] {
        assert_eq!(push_enter_x(dir, 0.0), dir, "进场起点在屏外整幅处");
        assert_eq!(push_enter_x(dir, 1.0), 0.0, "进场终点精确归零");
        assert_eq!(push_exit_x(dir, 0.0), 0.0, "出场起点在原位");
        assert_eq!(push_exit_x(dir, 1.0), -dir, "出场终点整幅移出");
        // 越界进度钳住,不许滑过头
        assert_eq!(push_enter_x(dir, 1.5), 0.0);
        assert_eq!(push_exit_x(dir, -0.5), 0.0);
        for i in 0..=10 {
            let d = i as f32 / 10.0;
            let gap = push_enter_x(dir, d) - push_exit_x(dir, d);
            assert!(
                (gap - dir).abs() < 1e-6,
                "两层间距恒为一幅: dir={dir} d={d}"
            );
        }
    }
}

/// 进场动画属原版 reduce 段**点名豁免**的那一档:开着减弱动效照播。
#[test]
fn 进场动画不受减弱动效影响() {
    const {
        assert!(
            !mt_ui::motion::PANE_ENTER.respects_reduce,
            "styles.css:441-443 明确豁免 .pane-enter"
        );
    }
    assert_eq!(
        mt_ui::motion::PANE_ENTER.duration,
        std::time::Duration::from_millis(260),
        "--motion-pane-enter: 0.26s"
    );
    crate::motion::with_reduce(true, || {
        let spec = mt_ui::motion::PANE_ENTER;
        assert!(spec.running_at(std::time::Duration::ZERO, true));
        assert!(spec.progress_at(std::time::Duration::ZERO, true) < 0.01);
    });
}

/// 还原飞行层与铺满态的终端主体逐像素同高(此前飞行层漏了铺满组自己那条 tab 栏,
/// 终端凭空高出 `TAB_BAR_H`,还原第一帧多一次 PTY resize、画面跳 30px)。
///
/// 铺满态:`render_leaf`(tab 栏 + 主体)占 flex_1,折叠区按内容撑开、顶到上限就滚;
/// 飞行层:同一个 `render_leaf` + 按 [`collapsed_zone_height`] 占位。两边主体都是
/// 「整幅 − tab 栏 − 折叠区」。
#[test]
fn 还原飞行层与铺满态终端主体等高() {
    for area_h in [240.0_f32, 600.0, 853.5, 1400.0] {
        for bars in 1..=12 {
            // 铺满态折叠区:每条 TAB_BAR_H 的内容高,max_h 封顶
            let zone_in_maximized = (bars as f32 * TAB_BAR_H).min(collapsed_zone_max(area_h));
            let zone_in_flight = collapsed_zone_height(area_h, bars);
            assert_eq!(zone_in_flight, zone_in_maximized, "h={area_h} bars={bars}");
            let body_maximized = area_h - TAB_BAR_H - zone_in_maximized;
            let body_in_flight = area_h - TAB_BAR_H - zone_in_flight;
            assert_eq!(body_in_flight, body_maximized);
            // 旧写法(飞行层只有主体 + 占位)差的正是一条 tab 栏
            let old_body = area_h - zone_in_flight;
            assert!((old_body - body_maximized - TAB_BAR_H).abs() < 1e-3);
        }
    }
    // 上限至少一条的高(终端区很矮时也放得下一条折叠条)
    assert_eq!(collapsed_zone_max(10.0), TAB_BAR_H);
}

/// 最大化状态下分屏:`clear_maximized` 与叶子 +1 落在同一帧,还原飞行层的折叠区
/// 必须按上一帧(铺满态真正画出来的)条数占位,终端主体才与铺满态等高。
#[test]
fn 还原同帧叶子数变化时折叠条按上一帧算() {
    use crate::tree::PaneState;
    let leaf = |name: &str| SplitNode::leaf(PaneState::new(name));
    let split = |children: Vec<SplitNode>| SplitNode::Split {
        id: crate::tree::gen_id("split"),
        direction: SplitDirection::Horizontal,
        sizes: vec![],
        children,
    };
    let (a, b) = (leaf("a"), leaf("b"));
    // 铺满 a、底部一条 b 的折叠条;在 a 上分屏 → 同一帧还原且多出一格
    let before = split(vec![a.clone(), b.clone()]);
    let after = split(vec![split(vec![a.clone(), leaf("new")]), b.clone()]);
    assert_eq!(restore_bar_count(Some(&before), &after), 1);
    // 旧写法按变更后的布局数:多一条
    assert_eq!(after.leaves().len() - 1, 2);
    let area_h = 600.0_f32;
    let zone_in_maximized = (1.0 * TAB_BAR_H).min(collapsed_zone_max(area_h));
    let bars = restore_bar_count(Some(&before), &after);
    assert_eq!(collapsed_zone_height(area_h, bars), zone_in_maximized);
    // 布局没变的还原:两种取法一致
    assert_eq!(restore_bar_count(Some(&before), &before), 1);
    // 上一帧快照缺席时退回现布局
    assert_eq!(restore_bar_count(None, &after), 2);
}

/// 切 tab / 关激活 tab 的推移方向与出场层取舍(见 [`tab_swap_plan`])。
#[test]
fn 切tab与关激活tab的推移方向() {
    // 普通切 tab:往右切从右进,往左切从左进,两层都画
    assert_eq!(tab_swap_plan(Some(0), 0, 2), (1.0, true));
    assert_eq!(tab_swap_plan(Some(2), 2, 0), (-1.0, true));
    // [A, B, C] 关掉激活的 B(上一帧下标 1):
    // 右邻 C 顶到下标 1 → 从右推入;左邻 A 在下标 0 → 从左推入;都只播进场
    assert_eq!(tab_swap_plan(None, 1, 1), (1.0, false));
    assert_eq!(tab_swap_plan(None, 1, 0), (-1.0, false));
    // 关掉最后一个(下标 2),激活退到左邻(下标 1):从左推入
    assert_eq!(tab_swap_plan(None, 2, 1), (-1.0, false));
    // 「关闭其他」从最左的 A 发起、激活的 C(下标 2)被关:A 从左推入
    assert_eq!(tab_swap_plan(None, 2, 0), (-1.0, false));
}

/// 叶内切 tab 的过渡只认「上一轮 render 刚画过这一叶」时发生的变化。
///
/// 最大化 A、在 B 的折叠条上右键关掉 B 的激活 tab(折叠条不走 `render_leaf`,记录停在
/// 最大化之前),还原时 B 再以展开态画到:记录是好几轮之前的,只对齐不横推。
#[test]
fn 隔了几轮才再画到的叶子不起切tab过渡() {
    // 上一轮刚画过(普通点 tab / 关 tab):起过渡
    assert!(leaf_drawn_recently(41, 42));
    // 这一轮已经画过一次(还原飞行层与整树各画一遍):同样算
    assert!(leaf_drawn_recently(42, 42));
    // 中间隔了至少一轮没画到(折叠在最大化里 / 切去了别的面板、项目):只对齐
    assert!(!leaf_drawn_recently(40, 42));
    assert!(!leaf_drawn_recently(3, 1_000));
    // 代次溢出边界不 panic
    assert!(leaf_drawn_recently(u64::MAX, u64::MAX));
}

/// 关分屏后邻居扩张只认「叶子只少不多」。
#[test]
fn 只有关掉分屏才算叶子只少不多() {
    use crate::tree::PaneState;
    let leaf = |name: &str| SplitNode::leaf(PaneState::new(name));
    let (a, b, c) = (leaf("a"), leaf("b"), leaf("c"));
    let split = |children: Vec<SplitNode>| SplitNode::Split {
        id: crate::tree::gen_id("split"),
        direction: SplitDirection::Horizontal,
        sizes: vec![],
        children,
    };
    let three = split(vec![a.clone(), b.clone(), c.clone()]);
    let two = split(vec![a.clone(), c.clone()]);
    assert!(leaves_only_shrank(&three, &two), "关掉 b");
    assert!(leaves_only_shrank(&two, &a), "关到只剩一格");
    assert!(!leaves_only_shrank(&two, &three), "分屏是多了一格");
    assert!(!leaves_only_shrank(&two, &two), "没增没减");
    // 有增有减(把 c 拖出去在别处新分了一格):不算
    let moved = split(vec![a.clone(), leaf("d")]);
    assert!(!leaves_only_shrank(&three, &moved));
}

/// 子节点数与存的百分比对不上 / 有非法值 → 均分,不许拿 0 去乘。
#[test]
fn 比例对不上时均分() {
    assert_eq!(split_fractions(&[30.0, 70.0], 3), vec![1.0 / 3.0; 3]);
    assert_eq!(split_fractions(&[], 2), vec![0.5, 0.5]);
    assert_eq!(split_fractions(&[0.0, 100.0], 2), vec![0.5, 0.5]);
    assert_eq!(split_fractions(&[f64::NAN, 1.0], 2), vec![0.5, 0.5]);
    assert!(split_fractions(&[50.0], 0).is_empty());
}

/// 拖完的像素换回百分比,和恒为 100。
#[test]
fn 像素换回百分比和为一百() {
    let pct = sizes_to_percent(&[300.0, 700.0]).unwrap();
    assert!((pct[0] - 30.0).abs() < 1e-9 && (pct[1] - 70.0).abs() < 1e-9);
    assert!((pct.iter().sum::<f64>() - 100.0).abs() < 1e-9);
}

/// 还没量出来 / 全是 0 时不写回 —— 写进去下次恢复就全退化成均分了。
#[test]
fn 总和非正时不写回() {
    assert!(sizes_to_percent(&[0.0, 0.0]).is_none());
    assert!(sizes_to_percent(&[]).is_none());
    assert!(sizes_to_percent(&[f64::NAN]).is_none());
}

/// tab 右键菜单的项序(没有 AI 会话的那一支:分支段整段不出):
/// 改名 / 复制 → 分屏 → 最大化 → 关闭一族 → 重新打开已关闭的,四条分隔线。
#[test]
fn tab_右键菜单项序() {
    use TabMenuAction::*;
    let actions = tab_menu_actions(false, false);
    assert_eq!(
        actions,
        vec![
            Some(Rename),
            Some(Duplicate),
            None,
            Some(SplitRight),
            Some(SplitDown),
            None,
            Some(ToggleMaximize),
            None,
            Some(CloseTab),
            Some(CloseOthers),
            Some(CloseToRight),
            Some(ClosePane),
            None,
            Some(ReopenClosed),
        ]
    );
    assert_eq!(actions.iter().filter(|a| a.is_none()).count(), 4);
}

/// 分支段的两种形态各自插在**分屏两项之后、最大化之前**,各带一条前导分隔线
/// (分支段本身逐条对照 `PaneGroup.tsx:354-372`)。
#[test]
fn tab_菜单分支段项序() {
    use TabMenuAction::*;
    let tail = [
        None,
        Some(ToggleMaximize),
        None,
        Some(CloseTab),
        Some(CloseOthers),
        Some(CloseToRight),
        Some(ClosePane),
        None,
        Some(ReopenClosed),
    ];
    // 有会话身份 + 有 fork 能力位
    let mut expected = vec![
        Some(Rename),
        Some(Duplicate),
        None,
        Some(SplitRight),
        Some(SplitDown),
        None,
        Some(ForkSession),
        Some(ViewSessionBranches),
    ];
    expected.extend(tail);
    assert_eq!(tab_menu_actions(true, false), expected);

    // 输入检测认出 AI 但没拿到 hook 身份 —— 置灰提示占同一个位置
    let mut expected = vec![
        Some(Rename),
        Some(Duplicate),
        None,
        Some(SplitRight),
        Some(SplitDown),
        None,
        Some(ForkNeedsIdentity),
    ];
    expected.extend(tail);
    assert_eq!(tab_menu_actions(false, true), expected);

    // 两者互斥(有身份就不会缺身份),真同时为真也不该塌成畸形菜单:
    // 段与段之间各自带分隔线,关闭段与「重新打开」照旧在最后
    let both = tab_menu_actions(true, true);
    assert_eq!(both.last(), Some(&Some(ReopenClosed)));
    assert_eq!(both.iter().filter(|a| a.is_none()).count(), 6);
}

/// 「最大化 / 还原」与控制条那颗按钮同一判据:单格布局用不上;正铺满按**组**认
/// (铺满后组内切过 tab 也算);别的组正铺满时,这一项是「最大化」(接管铺满);
/// 不在活动面板布局里的 pane(终端列表里别的面板)用不上。
#[test]
fn tab_菜单最大化一项的状态() {
    use crate::tree::{PaneState, gen_id};
    let a = PaneState::new("a");
    let a2 = PaneState::new("a2");
    let b = PaneState::new("b");
    let (a_id, a2_id, b_id) = (a.id.clone(), a2.id.clone(), b.id.clone());
    let mut left = SplitNode::leaf(a);
    left.append_pane(Some(&a_id), a2);
    let split = SplitNode::Split {
        id: gen_id("split"),
        direction: SplitDirection::Horizontal,
        sizes: vec![50.0, 50.0],
        children: vec![left, SplitNode::leaf(b)],
    };

    assert_eq!(maximize_menu_state(Some(&split), None, &a_id), Some(false));
    assert_eq!(
        maximize_menu_state(Some(&split), Some(&a_id), &a_id),
        Some(true)
    );
    assert_eq!(
        maximize_menu_state(Some(&split), Some(&a_id), &a2_id),
        Some(true),
        "同组另一个 tab:还是「还原」"
    );
    assert_eq!(
        maximize_menu_state(Some(&split), Some(&a_id), &b_id),
        Some(false),
        "别的组铺满着:接管"
    );
    assert_eq!(
        maximize_menu_state(Some(&split), None, "别的面板上的"),
        None
    );

    let single = SplitNode::leaf(PaneState::new("x"));
    let x = single.first_active_pane().unwrap().id.clone();
    assert_eq!(
        maximize_menu_state(Some(&single), None, &x),
        None,
        "单格布局"
    );
    assert_eq!(maximize_menu_state(None, None, &x), None);
}

/// 菜单显隐的判据与 `session_branch` 那份纯逻辑同源 —— 两处漂了就会出现
/// 「菜单出了 fork 项、点下去什么都不发生」。
#[test]
fn tab_菜单分支段判据取自能力位表() {
    use crate::tree::AiSessionRef;
    let id = "0199a1b2-c3d4-7e8f-9012-3456789abcde";
    let session = AiSessionRef {
        agent: Some("claude-code".into()),
        session_id: id.into(),
        cwd: None,
    };
    assert!(matches!(
        branch_menu_segment(Some(&session), None),
        BranchMenuSegment::Fork { .. }
    ));
    // 输入检测到 claude 但没身份 → 置灰提示那一支
    assert_eq!(
        branch_menu_segment(None, Some("claude")),
        BranchMenuSegment::NeedsIdentity
    );
    // 普通 shell:整段不出
    assert_eq!(branch_menu_segment(None, None), BranchMenuSegment::None);
}

/// 页签右键菜单与控制条提示里的快捷键一律按 id 取键位表(此刻生效的那份):id 写错会
/// 静默变成空串(菜单不标键位、提示里只剩「未设置」),这里逐个钉住都取得到,
/// 并确认 tab 菜单与控制条显示的是同一组键。
#[test]
fn 控制条提示里的快捷键取得到() {
    for id in [
        "terminalSearch",
        "splitRight",
        "splitDown",
        "closePane",
        "renamePane",
        // 最大化钮的提示、页签右键「关闭此终端 / 最大化 / 重新打开」的快捷键列
        "toggleMaximize",
        "closeTab",
        "reopenClosed",
        // 标记按钮的提示
        "markerPrev",
        "markerNext",
    ] {
        assert!(
            !crate::hotkeys::hotkey_label(id).is_empty(),
            "键位表里没有 {id}"
        );
        assert_eq!(
            crate::hotkeys::hotkey_tip(id),
            crate::hotkeys::hotkey_label(id),
            "{id} 没解绑时提示与菜单同一串"
        );
    }
    if cfg!(not(target_os = "macos")) {
        assert_eq!(crate::hotkeys::hotkey_label("renamePane"), "F2");
        assert_eq!(crate::hotkeys::hotkey_label("splitRight"), "Ctrl+Shift+D");
        assert_eq!(crate::hotkeys::hotkey_label("splitDown"), "Ctrl+Shift+E");
        assert_eq!(crate::hotkeys::hotkey_label("closePane"), "Ctrl+Shift+W");
    }
    let tip = tr!(
        "paneGroup",
        "splitRightTooltip",
        shortcut = crate::hotkeys::hotkey_tip("splitRight")
    );
    assert!(!tip.contains('{'), "占位符没换干净: {tip}");
    for key in ["maximizePane", "restorePane"] {
        let tip = tr!(
            "paneGroup",
            key,
            shortcut = crate::hotkeys::hotkey_tip("toggleMaximize")
        );
        assert!(!tip.contains('{'), "占位符没换干净: {tip}");
        assert!(tip.contains(&crate::hotkeys::hotkey_label("toggleMaximize")));
    }
    let tip = tr!(
        "paneGroup",
        "markerTooltip",
        prev = crate::hotkeys::hotkey_tip("markerPrev"),
        next = crate::hotkeys::hotkey_tip("markerNext")
    );
    assert!(!tip.contains('{'), "占位符没换干净: {tip}");
    assert!(tip.contains(&crate::hotkeys::hotkey_label("markerNext")));
}

/// marker 按钮的锚点由控件簇的布局常量算出(原版是量 DOM 矩形)。
/// 加减控件时这条会提醒同步改 [`MARKER_ANCHOR_INSET`] / [`marker_anchor_inset`]。
#[test]
fn 标记浮层锚点按控件簇布局算() {
    // 右侧簇:px-6 + 五个 22×22 方钮(查找/命令库/分屏右/分屏下/关整组,各带
    // 2px gap)+ marker 自己的 4px 右边距;查找、命令库钮与 marker 同以
    // 「有 pty」为前提,浮层用到锚点时五钮必然齐
    assert_eq!(MARKER_ANCHOR_INSET, 6.0 + 5.0 * 24.0 + 4.0);
    assert_eq!(MARKER_ANCHOR_INSET, 130.0);
    assert_eq!(marker_anchor_inset(false), 130.0);
    // 分了屏时簇里多一颗「最大化 / 还原」(22 + 2 gap),锚点跟着往左让
    assert_eq!(marker_anchor_inset(true), 130.0 + 24.0);
    assert_eq!(marker_anchor_inset(true), 154.0);
    // 面板右缘贴按钮右缘 → 左缘 = 叶右缘 - inset - 面板宽
    let leaf_right = 1000.0_f32;
    let left = leaf_right - marker_anchor_inset(false) - MARKER_PANEL_WIDTH;
    assert_eq!(left, 1000.0 - 130.0 - 300.0);
}

/// 命令库钮的锚点:它右边只剩分屏右 / 分屏下 / 关整组三颗(+ 条件出现的最大化)。
#[test]
fn 命令库浮层锚点按控件簇布局算() {
    assert_eq!(CMD_ANCHOR_INSET, 6.0 + 3.0 * 24.0);
    assert_eq!(cmd_anchor_inset(false), 78.0);
    assert_eq!(cmd_anchor_inset(true), 102.0);
    // 命令库钮紧挨在查找钮右侧:两者右缘差正好一颗钮 + gap,marker 再往左
    // 4px 边距 + 查找钮一颗
    assert_eq!(
        MARKER_ANCHOR_INSET - cmd_anchor_inset(false),
        2.0 * (CTRL_BTN + CTRL_GAP) + MARKER_BTN_MARGIN_RIGHT
    );
}

/// SSH 交互按钮夹在 marker 与查找之间:在场时 marker 锚点多让出「按钮宽 + gap」,
/// 按钮宽按条数的位数定死(1~500 条最多三位);不在场一点不让。
#[test]
fn ssh按钮在场时marker锚点让出它的宽度() {
    assert_eq!(ssh_anchor_extra(None), 0.0);
    let one_digit = 2.0 * SSH_BTN_PAD_X + CTRL_ICON + SSH_BTN_GAP + SSH_BTN_DIGIT_W;
    assert_eq!(ssh_button_width(1), one_digit);
    assert_eq!(ssh_button_width(9), one_digit);
    assert_eq!(ssh_button_width(10), one_digit + SSH_BTN_DIGIT_W);
    assert_eq!(ssh_button_width(500), one_digit + 2.0 * SSH_BTN_DIGIT_W);
    assert_eq!(ssh_anchor_extra(Some(42)), ssh_button_width(42) + CTRL_GAP);
    // 命令库钮在 SSH 钮右边,它的锚点与 SSH 钮无关
    assert_eq!(cmd_anchor_inset(false), CMD_ANCHOR_INSET);
}

/// 浮层里那些写死的像素尺寸**必须跟着界面字号一起缩放**。
///
/// 回归:写死 `px` 的那一版有实际后果 —— 用户把 `uiFontSize` 调大之后,
/// 36px 的时间列装不下 `15:29`,五个字符被折成了两行。
#[test]
fn 标记浮层尺寸跟着界面字号缩放() {
    // 默认基准下就是常量本身(所以锚点那条测试拿裸常量算仍然成立)
    assert_eq!(f32::from(crate::ui::font_px(MARKER_TIME_W)), MARKER_TIME_W);
    assert_eq!(f32::from(crate::ui::font_px(MARKER_SEQ_W)), MARKER_SEQ_W);

    // 调到滑块上限:三个尺寸都得跟着变大,不能纹丝不动
    crate::ui::set_ui_font(20.0, None);
    for (base, name) in [
        (MARKER_TIME_W, "时间列"),
        (MARKER_SEQ_W, "序号列"),
        (MARKER_PANEL_WIDTH, "面板宽"),
    ] {
        let scaled = f32::from(crate::ui::font_px(base));
        assert!(scaled > base, "{name}在大字号下必须变宽,实际 {scaled}");
    }
    // 「装不装得下」的真正判据是**列宽相对正文字号的倍数**,它必须恒定
    let ratio = f32::from(crate::ui::font_px(MARKER_TIME_W)) / f32::from(crate::ui::font_px(12.0));
    assert!(
        (ratio - MARKER_TIME_W / 12.0).abs() < 0.01,
        "列宽与字号脱钩了,实际倍数 {ratio}"
    );

    crate::ui::set_ui_font(13.0, None);
}

/// 浮层的存活判据:pane 还在、还是激活 tab、pty 没换。
#[test]
fn 切换激活_tab_后浮层判定为该关() {
    use crate::tree::PaneState;

    let mut first = PaneState::new("pwsh");
    first.pty_id = Some(7);
    let first_id = first.id.clone();
    let mut layout = SplitNode::leaf(first);

    let mut second = PaneState::new("pwsh");
    second.pty_id = Some(8);
    let second_id = second.id.clone();
    layout.append_pane(Some(&first_id), second);

    // append_pane 会把新 tab 设成激活的 → 原来那条浮层该关
    assert!(layout.activate_pane(&first_id));
    assert!(marker_popover_alive(&layout, &first_id, 7));
    assert!(
        !marker_popover_alive(&layout, &second_id, 8),
        "不是激活 tab"
    );

    assert!(layout.activate_pane(&second_id));
    assert!(!marker_popover_alive(&layout, &first_id, 7), "切走了就该关");
    assert!(marker_popover_alive(&layout, &second_id, 8));

    // pty 换了(重连 / 重建)同样算不在了
    assert!(!marker_popover_alive(&layout, &second_id, 99));
    // pane 压根不在布局里
    assert!(!marker_popover_alive(&layout, "pane-nonexistent", 7));
}

/// 键盘调比例一步 = 容器主轴的 5%,只在分隔线两侧那两格之间挪,两格都不小于最小格宽;
/// 顶到头就不挪(返回 None),容器还没量出来也不挪。
#[test]
fn 键盘调比例一步与最小格宽() {
    let sizes = [400.0, 600.0];
    // 朝后一格挪(→ / ↓):前一格变大 50px(1000 × 5%)
    assert_eq!(
        nudged_panel_size(&sizes, 0, 1000.0, true, 100.0),
        Some(450.0)
    );
    assert_eq!(
        nudged_panel_size(&sizes, 0, 1000.0, false, 100.0),
        Some(350.0)
    );
    // 后一格只剩 120:最多让到 100(最小格宽),不越过
    assert_eq!(
        nudged_panel_size(&[880.0, 120.0], 0, 1000.0, true, 100.0),
        Some(900.0)
    );
    assert_eq!(
        nudged_panel_size(&[900.0, 100.0], 0, 1000.0, true, 100.0),
        None,
        "已经顶到最小"
    );
    assert_eq!(
        nudged_panel_size(&[100.0, 900.0], 0, 1000.0, false, 100.0),
        None
    );
    // 三格里挪中间那条:只动第 1、2 格,第 0 格不受牵连(返回的是第 1 格的新宽)
    assert_eq!(
        nudged_panel_size(&[300.0, 300.0, 400.0], 1, 1000.0, true, 100.0),
        Some(350.0)
    );
    // 两格合起来都放不下两个最小格 / 容器没量出来 / 下标越界
    assert_eq!(
        nudged_panel_size(&[90.0, 90.0], 0, 180.0, true, 100.0),
        None
    );
    assert_eq!(nudged_panel_size(&sizes, 0, 0.0, true, 100.0), None);
    assert_eq!(nudged_panel_size(&sizes, 1, 1000.0, true, 100.0), None);
}

/// 双击均分:两侧两格各取一半;已经等宽不动。
#[test]
fn 双击分隔条均分两侧() {
    assert_eq!(equalized_panel_size(&[300.0, 700.0], 0), Some(500.0));
    assert_eq!(
        equalized_panel_size(&[200.0, 300.0, 500.0], 1),
        Some(400.0),
        "只均分那条分隔线两侧"
    );
    assert_eq!(equalized_panel_size(&[500.0, 500.4], 0), None);
    assert_eq!(equalized_panel_size(&[500.0], 0), None);
}

/// 点击位置反推分隔线下标:取最近的那条;一格的 split 没有分隔线。
#[test]
fn 按点击位置认分隔线() {
    // 起点 100,三格 200/300/500 → 分隔线在 300 与 600
    let sizes = [200.0, 300.0, 500.0];
    assert_eq!(nearest_boundary(&sizes, 100.0, 299.0), Some(0));
    assert_eq!(nearest_boundary(&sizes, 100.0, 303.0), Some(0));
    assert_eq!(nearest_boundary(&sizes, 100.0, 597.0), Some(1));
    assert_eq!(nearest_boundary(&sizes, 100.0, 9999.0), Some(1));
    assert_eq!(nearest_boundary(&[1000.0], 0.0, 10.0), None);
}

/// split 自己的矩形:双击认分隔线时要 split 的起点(嵌套里也要算对)。
#[test]
fn 分屏节点矩形按比例换算() {
    use crate::tree::{PaneState, gen_id};
    let leaf = |name: &str| SplitNode::leaf(PaneState::new(name));
    let inner_id = gen_id("split");
    let inner = SplitNode::Split {
        id: inner_id.clone(),
        direction: SplitDirection::Vertical,
        sizes: vec![50.0, 50.0],
        children: vec![leaf("b"), leaf("c")],
    };
    let root = SplitNode::Split {
        id: gen_id("split"),
        direction: SplitDirection::Horizontal,
        sizes: vec![30.0, 70.0],
        children: vec![leaf("a"), inner],
    };
    let r = node_rect_in(&root, &inner_id, 0.0, 0.0, 1000.0, 800.0).unwrap();
    assert_eq!(
        (r.left, r.top, r.width, r.height),
        (300.0, 0.0, 700.0, 800.0)
    );
    let whole = node_rect_in(&root, root.id(), 0.0, 0.0, 1000.0, 800.0).unwrap();
    assert_eq!((whole.left, whole.width), (0.0, 1000.0));
}

/// 激活页签跟随:变了才滚、只滚一次;没排过版先挂着,排过版的那一帧补上;
/// 同一个页签被挪了位置也算变(要跟过去)。
#[test]
fn 激活页签只在变化时滚一次() {
    let mut follow = TabFollow::default();
    // 刚建出来、还没排过版:先挂着
    assert!(!follow.observe("p1", 0, false));
    assert!(follow.observe("p1", 0, true), "排过版的下一帧补滚");
    // 没变:用户自己滚页签区,不许拽回去
    assert!(!follow.observe("p1", 0, true));
    assert!(!follow.observe("p1", 0, true));
    // 切到别的页签:滚一次
    assert!(follow.observe("p9", 8, true));
    assert!(!follow.observe("p9", 8, true));
    // 同一个页签挪了位置(Ctrl+Shift+PgDn / 左边关掉一个)
    assert!(follow.observe("p9", 7, true));
    assert!(!follow.observe("p9", 7, true));
}

/// 换算一圈回来不变形:百分比 → 像素 → 百分比。
#[test]
fn 百分比像素往返不变形() {
    let stored = [20.0, 55.0, 25.0];
    let area = 1234.0_f64;
    let pixels: Vec<f64> = split_fractions(&stored, 3)
        .iter()
        .map(|f| f * area)
        .collect();
    let back = sizes_to_percent(&pixels).unwrap();
    for (a, b) in stored.iter().zip(back.iter()) {
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }
}

/// 单时钟裁剪窗插值:正反方向端点精确到位,越界 delta 被钳住不外溢。
#[test]
fn 最大化裁剪窗矩形插值端点精确且越界钳住() {
    let from = RevealFrom {
        left: 320.0,
        top: 180.0,
        width: 480.0,
        height: 360.0,
    };
    let full = RevealFrom {
        left: 0.0,
        top: 0.0,
        width: 1000.0,
        height: 700.0,
    };

    // 展开:from → full
    let start = reveal_clip_rect(from, full, 0.0);
    assert_eq!(
        (start.left, start.top, start.width, start.height),
        (320.0, 180.0, 480.0, 360.0)
    );
    let mid = reveal_clip_rect(from, full, 0.5);
    assert_eq!(
        (mid.left, mid.top, mid.width, mid.height),
        (160.0, 90.0, 740.0, 530.0)
    );
    let end = reveal_clip_rect(from, full, 1.0);
    assert_eq!(
        (end.left, end.top, end.width, end.height),
        (0.0, 0.0, 1000.0, 700.0)
    );

    // 越界输入被钳在 [0, 1]
    let under = reveal_clip_rect(from, full, -0.5);
    assert_eq!((under.left, under.width), (320.0, 480.0));
    let over = reveal_clip_rect(from, full, 1.5);
    assert_eq!((over.left, over.width), (0.0, 1000.0));

    // 还原:full → from
    let back_end = reveal_clip_rect(full, from, 1.0);
    assert_eq!(
        (back_end.left, back_end.top, back_end.width, back_end.height),
        (320.0, 180.0, 480.0, 360.0)
    );
}
