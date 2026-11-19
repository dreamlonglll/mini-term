//! 「注册 Hook 状态更准」的一次性提示。
//!
//! # 为什么要提示
//!
//! AI 状态的权威来源是 hook(AGENTS.md「AI 状态判定」),但 `hookEnabled` 缺省关着、hook
//! 也要用户自己去「设置 → AI → Hook 事件」注册。没人告诉的话,用户一直用的是输入检测的降级
//! 路径:状态靠猜、完成播报靠回合闩,准确度差一截,而他们并不知道还有更准的办法。
//!
//! # 什么时候提示
//!
//! ```text
//! store 发 PaneStatusChanged
//!   → 配置里已置位 / 本次运行已有结论 / 正在读注册现状 → 什么都不做
//!   → 找一个处于 AI 会话、且这家支持 hook 的 pane(pick_hint_agent)→ 没有就等下次
//!   → [后台] 读各家 hook 注册现状(mt_ai::hook_registry,读盘)
//!   → hint_decision:
//!        有任何一家注册过 → 视为已知晓:不提示,直接置位
//!        一家都没注册     → 弹 toast(点击直达 Hook 事件页),置位
//! ```
//!
//! 一家都没注册时,这个 pane 的状态**只可能**来自输入检测(没有 hook 能上报),所以这里不必
//! 另外分辨「这次 AI 会话是哪条路径认出来的」;反过来,只要注册过任何一家,不管总开关此刻开
//! 没开,用户都已经知道有这回事 —— 这两条合起来,`hookEnabled` 那个总开关就不进判据了。
//! opencode 这类只靠输入检测的 agent 没有 hook 可注册,提示它们只会让人白跑一趟设置页,
//! 不提示也不置位,等下一家支持 hook 的出现。
//!
//! **一辈子一次**:判出结论就落 `AppConfig::hook_hint_shown`,之后启动直接跳过。
//!
//! # 落点
//!
//! 整个判断都在 store 之外:订阅 store 已有的 `PaneStatusChanged`,读 pane 上现成的状态与
//! agent,读盘丢后台执行器 —— store 里只多了置位那一个 setter。

use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, Task};
use mt_ai::hook_registry::{HookAgent, HookRegistrationInfo};

use crate::i18n::{t, tr};
use crate::settings::SettingsPage;
use crate::store::{AppStore, StoreEvent};
use crate::tree::PaneStatus;

/// 判出来的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintDecision {
    /// 用户注册过某一家的 hook:已经知道这回事,不提示,直接置位。
    AlreadyKnown,
    /// 弹提示(带上那家的展示名),并置位。
    Show(&'static str),
}

/// 这家 agent 支持 hook 的话给出它的展示名(提示文案用);opencode 之类只靠输入检测的、
/// 认不出的,返回 `None`。
fn hook_capable_label(agent: &str) -> Option<&'static str> {
    let kind = mt_ai::AgentKind::parse(agent)?;
    HookAgent::ALL
        .iter()
        .find(|hook| hook.agent() == kind)
        .map(|hook| hook.label())
}

/// 在一组 pane 的 `(状态, agent)` 里挑一个该提示的:正处于 AI 会话(ai-working / ai-idle)
/// 且这家支持 hook。返回它的展示名;一个都没有返回 `None`。
pub fn pick_hint_agent<'a>(
    panes: impl IntoIterator<Item = (PaneStatus, Option<&'a str>)>,
) -> Option<&'static str> {
    panes.into_iter().find_map(|(status, agent)| {
        if !matches!(status, PaneStatus::AiWorking | PaneStatus::AiIdle) {
            return None;
        }
        hook_capable_label(agent?)
    })
}

/// 读到注册现状之后该怎么办(判据见模块注释「什么时候提示」)。`label` 是
/// [`pick_hint_agent`] 挑出来的那家。
pub fn hint_decision(label: &'static str, registrations: &[HookRegistrationInfo]) -> HintDecision {
    if registrations.iter().any(|reg| reg.registered > 0) {
        HintDecision::AlreadyKnown
    } else {
        HintDecision::Show(label)
    }
}

/// 提示的状态机。实体由 [`install`] 建出来挂在全局,活到窗口关闭。
struct HookHint {
    store: Entity<AppStore>,
    /// 本次运行已经有结论(提示过 / 置过位 / 配置里早就置位),之后什么都不做。
    settled: bool,
    /// 正在后台读注册现状。读回来之前不再起第二趟。
    checking: Option<Task<()>>,
    _sub: Subscription,
}

/// 全局只为保活:实体一 drop,订阅跟着断。
struct GlobalHookHint {
    _hint: Entity<HookHint>,
}
impl Global for GlobalHookHint {}

/// 装上提示的观察者。启动时调一次(`Workspace::new`)。
pub fn install(store: Entity<AppStore>, cx: &mut App) {
    let entity = cx.new(|cx: &mut Context<HookHint>| {
        let sub = cx.subscribe(&store, |this: &mut HookHint, _, event: &StoreEvent, cx| {
            if matches!(event, StoreEvent::PaneStatusChanged) {
                this.on_pane_status(cx);
            }
        });
        let settled = store.read(cx).config().hook_hint_shown;
        HookHint {
            store,
            settled,
            checking: None,
            _sub: sub,
        }
    });
    cx.set_global(GlobalHookHint { _hint: entity });
}

impl HookHint {
    fn on_pane_status(&mut self, cx: &mut Context<Self>) {
        if self.settled || self.checking.is_some() {
            return;
        }
        let label = {
            let store = self.store.read(cx);
            if store.config().hook_hint_shown {
                self.settled = true;
                return;
            }
            let panes = store
                .projects()
                .iter()
                .filter_map(|project| store.project_state(&project.id))
                .flat_map(|state| state.all_panes())
                .map(|pane| (pane.status, pane.ai_agent()));
            pick_hint_agent(panes)
        };
        let Some(label) = label else {
            return;
        };
        // 读各家配置文件(Claude 的 settings.json、Codex 的 hooks.json……),丢后台
        self.checking = Some(cx.spawn(async move |this, cx| {
            let registrations = cx
                .background_executor()
                .spawn(async { mt_ai::hook_registry::get_ai_hook_registrations() })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.checking = None;
                this.conclude(hint_decision(label, &registrations), cx);
            });
        }));
    }

    fn conclude(&mut self, decision: HintDecision, cx: &mut Context<Self>) {
        if self.settled {
            return;
        }
        self.settled = true;
        // 读盘那一拍里可能已经被别处置过位,置位本身幂等;再判一次只为不重复弹提示
        let already = self.store.read(cx).config().hook_hint_shown;
        self.store
            .update(cx, |store, cx| store.mark_hook_hint_shown(cx));
        if already {
            return;
        }
        if let HintDecision::Show(label) = decision {
            crate::toast::push_settings_hint(
                t("app", "hookHint.title").to_string(),
                tr!("app", "hookHint.message", agent = label),
                SettingsPage::AiHook,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(agent: &str, registered: usize) -> HookRegistrationInfo {
        HookRegistrationInfo {
            agent: agent.into(),
            label: agent.into(),
            file: String::new(),
            registered,
            total: 8,
        }
    }

    /// 支持 hook 的六家给展示名;opencode 与认不出的不给。
    #[test]
    fn 支持_hook_的_agent_才提示() {
        assert_eq!(hook_capable_label("claude"), Some("Claude Code"));
        assert_eq!(hook_capable_label("claude-code"), Some("Claude Code"));
        assert_eq!(hook_capable_label("codex"), Some("Codex"));
        assert_eq!(hook_capable_label("grok"), Some("Grok"));
        assert_eq!(hook_capable_label("omp"), Some("oh-my-pi"));
        assert_eq!(hook_capable_label("pi"), Some("pi"));
        assert_eq!(hook_capable_label("agy"), Some("Antigravity CLI"));
        assert_eq!(hook_capable_label("opencode"), None);
        assert_eq!(hook_capable_label("bash"), None);
        assert_eq!(hook_capable_label(""), None);
    }

    /// 只挑处于 AI 会话的 pane;不在 AI 会话 / 没认出 agent / 只靠输入检测的一律跳过。
    #[test]
    fn 挑出处于_ai_会话且支持_hook_的那家() {
        assert_eq!(
            pick_hint_agent([
                (PaneStatus::Idle, Some("claude")),
                (PaneStatus::AiWorking, None),
                (PaneStatus::AiIdle, Some("opencode")),
                (PaneStatus::Error, Some("codex")),
                (PaneStatus::AiWorking, Some("codex")),
            ]),
            Some("Codex")
        );
        assert_eq!(
            pick_hint_agent([(PaneStatus::AiIdle, Some("opencode"))]),
            None,
            "opencode 没有 hook 可注册"
        );
        assert_eq!(
            pick_hint_agent([(PaneStatus::Idle, Some("claude"))]),
            None,
            "退出 AI 之后不算"
        );
        assert_eq!(pick_hint_agent(Vec::new()), None);
    }

    /// 一家都没注册才弹;注册过任意一家就算已知晓(哪怕不是眼下这家、哪怕总开关关着)。
    #[test]
    fn 注册过任意一家就不提示() {
        let none = [reg("claude", 0), reg("codex", 0), reg("grok", 0)];
        assert_eq!(hint_decision("Codex", &none), HintDecision::Show("Codex"));
        let other = [reg("claude", 16), reg("codex", 0)];
        assert_eq!(hint_decision("Codex", &other), HintDecision::AlreadyKnown);
        // 旧事件集(registered < total)也算注册过
        let stale = [reg("claude", 0), reg("codex", 3)];
        assert_eq!(hint_decision("Codex", &stale), HintDecision::AlreadyKnown);
        // 拿不到注册现状(读不到 home)= 按没注册算
        assert_eq!(hint_decision("pi", &[]), HintDecision::Show("pi"));
    }
}
