#!/usr/bin/env python3
"""为宣传片 e2e 准备一份隔离的数据目录(MT_APP_DATA_DIR)与演示 HOME 里的会话记录。

用法:seed.py <data_dir> <demo_home> <repo_root> [--theme blue-hour|morning-mist] [--hook]

- config.db / layout.db 按 mt-config / mt-layout 的表结构直接写(键值 settings + projects 行),
  省掉手点「添加项目」;
- themes/ 从仓库 theme/ 复制两款成品皮肤,customThemeId 选中其一;
- model-pricing.json 是离线价格裸表(容器里连不上 models.dev);
- {demo_home}/.claude/projects 与 .codex/sessions 下造一个月的演示会话记录,
  给「使用统计」与「AI 历史」面板出图。会话内容全是虚构的演示文本。
"""
import json
import os
import random
import shutil
import sqlite3
import sys
import time
import uuid
from datetime import datetime, timedelta, timezone

args = [a for a in sys.argv[1:] if not a.startswith('--')]
flags = [a for a in sys.argv[1:] if a.startswith('--')]
DATA, HOME, REPO = args[0], args[1], args[2]
THEME = 'blue-hour'
for i, a in enumerate(sys.argv):
    if a == '--theme':
        THEME = sys.argv[i + 1]
HOOK = '--hook' in flags
CODE = os.path.join(HOME, 'code')
TZ = timezone(timedelta(hours=8))
NOW = datetime(2026, 10, 3, 11, 20, tzinfo=TZ)
rnd = random.Random(20261003)

PROJECTS = [
    ('p-orbit', 'orbit-api', 'Rust 订单服务'),
    ('p-aurora', 'aurora-web', '实时数据看板'),
    ('p-infra', 'infra-k8s', None),
    ('p-mini-term', 'mini-term', None),
    ('p-pulse', 'pulse-ml', None),
    ('p-nebula', 'nebula-go', None),
]
TREE = [
    {'id': 'g-work', 'name': '工作', 'collapsed': False, 'children': ['p-orbit', 'p-aurora', 'p-infra']},
    {'id': 'g-oss', 'name': '开源', 'collapsed': False, 'children': ['p-mini-term']},
    {'id': 'g-lab', 'name': '实验', 'collapsed': False, 'children': ['p-pulse', 'p-nebula']},
]


def path_of(name):
    return os.path.join(CODE, name)


def leaf(*panes):
    return {'type': 'leaf', 'panes': [{'shellName': 'bash', **p} for p in panes]}


def split(direction, children, sizes):
    return {'type': 'split', 'direction': direction, 'children': children, 'sizes': sizes}


# 每个项目启动时还原的分屏树(SavedProjectLayout)。orbit-api 是主场景:左大右二。
LAYOUTS = {
    'p-orbit': {'tabs': [{'splitLayout': split('horizontal', [
        leaf({}),
        split('vertical', [leaf({}), leaf({})], [50.0, 50.0]),
    ], [58.0, 42.0])}], 'activeTabIndex': 0},
    'p-aurora': {'tabs': [{'splitLayout': split('horizontal', [leaf({}), leaf({})], [50.0, 50.0])}], 'activeTabIndex': 0},
    'p-mini-term': {'tabs': [{'splitLayout': leaf({})}], 'activeTabIndex': 0},
    'p-infra': {'tabs': [{'splitLayout': leaf({})}], 'activeTabIndex': 0},
}

SSH = [
    ('ssh-prod-1', 'prod-api-01', '10.20.1.11', 'deploy', '生产'),
    ('ssh-prod-2', 'prod-api-02', '10.20.1.12', 'deploy', '生产'),
    ('ssh-db', 'prod-db-primary', '10.20.2.5', 'dba', '生产'),
    ('ssh-stg', 'staging', 'staging.orbit.internal', 'dev', '测试'),
    ('ssh-gpu', 'gpu-box', '192.168.1.42', 'ml', '实验室'),
    ('ssh-pi', 'raspberry-pi', 'pi.local', 'pi', '实验室'),
]

COMMANDS = {
    'groups': ['构建', 'Git', '运维'],
    'commands': [
        {'id': 'c1', 'name': '全量测试', 'command': 'cargo test --workspace', 'group': '构建'},
        {'id': 'c2', 'name': 'Release 构建', 'command': 'cargo build --release', 'group': '构建'},
        {'id': 'c3', 'name': '前端开发服务器', 'command': 'npm run dev', 'group': '构建'},
        {'id': 'c4', 'name': '图形化日志', 'command': 'git log --graph --oneline --decorate -n 20', 'group': 'Git'},
        {'id': 'c5', 'name': '同步主干', 'command': 'git fetch origin && git rebase origin/main', 'group': 'Git'},
        {'id': 'c6', 'name': '查看 Pod', 'command': 'kubectl get pods -n orbit -o wide', 'group': '运维'},
        {'id': 'c7', 'name': '跟踪 API 日志', 'command': 'kubectl logs -f deploy/orbit-api -n orbit', 'group': '运维'},
        {'id': 'c8', 'name': '磁盘占用', 'command': 'du -sh * | sort -h', 'group': None},
    ],
}


def reset_dir(d):
    if os.path.exists(d):
        shutil.rmtree(d)
    os.makedirs(d)


def write_config():
    db = sqlite3.connect(os.path.join(DATA, 'config.db'))
    db.executescript('''
    PRAGMA journal_mode=WAL;
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS projects (id TEXT PRIMARY KEY, ord INTEGER NOT NULL, data TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS ssh_connections (id TEXT PRIMARY KEY, ord INTEGER NOT NULL, data TEXT NOT NULL);
    ''')
    db.executemany('INSERT OR REPLACE INTO meta VALUES (?,?)', [('schema_version', '1'), ('initialized', '1')])
    settings = {
        'locale': 'zh',
        'theme': 'dark' if THEME == 'blue-hour' else 'light',
        'customThemeId': THEME,
        'projectTree': TREE,
        'lastActiveProjectId': 'p-orbit',
        'defaultShell': 'bash',
        'availableShells': [{'name': 'bash', 'command': '/bin/bash', 'args': ['-l']}],
        'uiFontSize': 13.0,
        'uiFontFamily': 'Noto Sans CJK SC',
        'terminalFontSize': 13.5,
        'terminalFontFamily': 'JetBrains Mono, Noto Sans Mono CJK SC',
        'terminalLigatures': False,
        'terminalScrollback': 10000,
        'terminalFollowTheme': True,
        'aiCompletionPopup': True,
        'aiCompletionTaskbarFlash': False,
        'aiCompletionSound': False,
        'aiAttentionNotify': True,
        'editors': [],
        'gitChangesViewMode': 'list',
        'longPasteToFile': True,
        'longPasteLineThreshold': 10,
        'longPasteCharThreshold': 2000,
        'remotePasteDir': '.mini-term/pasted',
        'hookEnabled': HOOK,
        'smartCopyPaste': True,
        'sshGroups': ['生产', '测试', '实验室'],
        'commandLibrary': COMMANDS,
        'usageRange': 'days30',
        'terminalFpsForeground': 60,
        'terminalFpsBackground': 60,
        'mobileRelay': {'relayUrl': '', 'desktopKey': '', 'launchers': [
            {'id': 'claude', 'name': 'Claude', 'command': 'claude'},
            {'id': 'codex', 'name': 'Codex', 'command': 'codex'}]},
    }
    db.executemany('INSERT OR REPLACE INTO settings VALUES (?,?)',
                   [(k, json.dumps(v, ensure_ascii=False)) for k, v in settings.items()])
    rows = []
    for i, (pid, name, desc) in enumerate(PROJECTS):
        data = {'id': pid, 'name': name, 'path': path_of(name), 'expandedDirs': []}
        if desc:
            data['description'] = desc
        if pid == 'p-orbit':
            data['expandedDirs'] = [os.path.join(path_of(name), 'src'), os.path.join(path_of(name), 'src', 'routes')]
            data['sshConnectionGroups'] = ['生产']
        rows.append((pid, i, json.dumps(data, ensure_ascii=False)))
    db.executemany('INSERT OR REPLACE INTO projects VALUES (?,?,?)', rows)
    db.executemany('INSERT OR REPLACE INTO ssh_connections VALUES (?,?,?)', [
        (cid, i, json.dumps({'id': cid, 'name': n, 'host': h, 'port': 22, 'user': u, 'group': g}, ensure_ascii=False))
        for i, (cid, n, h, u, g) in enumerate(SSH)])
    db.commit()
    db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    db.close()


def write_layout():
    db = sqlite3.connect(os.path.join(DATA, 'layout.db'))
    db.executescript('''
    PRAGMA journal_mode=WAL;
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS app_layout (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS project_layout (project_id TEXT PRIMARY KEY, layout_json TEXT NOT NULL, updated_at_ms INTEGER NOT NULL);
    ''')
    db.executemany('INSERT OR REPLACE INTO meta VALUES (?,?)', [('schema_version', '1'), ('config_migrated', '1')])
    app = {
        'window': {'mode': 'windowed', 'x': 0, 'y': 0, 'width': 1920, 'height': 1080},
        'layoutSizes': [330.0, 1546.0],
        'middleColumnSizes': [404.0, 644.0],
        'middleColumnVisible': True,
        'rightDrawerWidth': 460.0,
    }
    db.executemany('INSERT OR REPLACE INTO app_layout VALUES (?,?)', [(k, json.dumps(v)) for k, v in app.items()])
    now_ms = int(time.time() * 1000)
    db.executemany('INSERT OR REPLACE INTO project_layout VALUES (?,?,?)',
                   [(pid, json.dumps(l), now_ms) for pid, l in LAYOUTS.items()])
    db.commit()
    db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    db.close()


def write_themes():
    themes = os.path.join(DATA, 'themes')
    os.makedirs(themes, exist_ok=True)
    for t in ('blue-hour', 'morning-mist'):
        shutil.copytree(os.path.join(REPO, 'theme', t), os.path.join(themes, t), dirs_exist_ok=True)


def write_pricing():
    # 演示用离线价格($/token)。数字只为让面板有金额可看,不代表任何官方报价。
    m = 1e-6
    table = {
        'claude-opus-5-5': {'input': 5 * m, 'output': 25 * m, 'cacheRead': 0.5 * m, 'cacheWrite': 6.25 * m},
        'claude-sonnet-5-5': {'input': 3 * m, 'output': 15 * m, 'cacheRead': 0.3 * m, 'cacheWrite': 3.75 * m},
        'claude-haiku-4-5': {'input': 1 * m, 'output': 5 * m, 'cacheRead': 0.1 * m, 'cacheWrite': 1.25 * m},
        'gpt-5-codex': {'input': 1.25 * m, 'output': 10 * m, 'cacheRead': 0.125 * m, 'cacheWrite': 0},
    }
    with open(os.path.join(DATA, 'model-pricing.json'), 'w') as f:
        json.dump(table, f, indent=2)


# ---------------------------------------------------------------- 会话记录
CLAUDE_TOPICS = {
    'orbit-api': ['修复订单查询的 N+1 问题', '给订单接口加限流中间件', '补齐 sqlx 迁移脚本', '排查 /api/orders 偶发 500',
                  '把健康检查拆成 liveness / readiness', '订单状态机重构', '给 429 响应补 Retry-After', '订单导出改成流式 CSV',
                  '接入 OpenTelemetry 链路追踪', '把配置迁到 figment', '为结算 Worker 加幂等键', '压测 /api/orders 并给出瓶颈',
                  '升级 axum 0.8 的路由写法', '订单事件改用 outbox 模式', '补齐集成测试的测试容器', '清理 clippy 警告'],
    'aurora-web': ['仪表盘改成 5 秒刷新', '给图表加深色主题', '升级到 React 19', '修复切换项目时的闪烁', '把状态管理换成 zustand',
                   '首屏加骨架屏', '给 Dashboard 写单测', '图表支持按小时聚合', '修复 Safari 下的布局错位', '接入 i18n'],
    'mini-term': ['终端页签标题跟随 shell', '使用统计支持时分秒', 'SSH 连接复制', 'Markdown 预览加 mermaid 放大', '命令库支持分组',
                  '全局搜索支持正则', '修复 ConPTY 宽字符列宽'],
    'infra-k8s': ['给 orbit-api 加 HPA', 'compose 加 redis', 'Terraform 拆模块', '给 ingress 加限速注解'],
    'pulse-ml': ['LSTM 基线调参', '加早停与学习率调度', '用 polars 重写特征工程'],
    'nebula-go': ['加 /healthz 与优雅退出', '实现一致性哈希路由'],
}
CODEX_TOPICS = {
    'aurora-web': ['优化 useMetrics 的缓存策略', '给 Dashboard 写单测'],
    'orbit-api': ['review: 限流中间件'],
    'nebula-go': ['实现 LRU 缓存'],
}
MODELS = [('claude-opus-5-5', 0.45), ('claude-sonnet-5-5', 0.45), ('claude-haiku-4-5-20251001', 0.10)]


def pick_model():
    x = rnd.random()
    acc = 0
    for name, w in MODELS:
        acc += w
        if x <= acc:
            return name
    return MODELS[-1][0]


def iso(dt):
    return dt.astimezone(timezone.utc).strftime('%Y-%m-%dT%H:%M:%S.') + f'{dt.microsecond // 1000:03d}Z'


def day_weight(d):
    # 工作日多、周末少,最近两周略涨 —— 让趋势图有起伏
    wd = d.weekday()
    base = 1.0 if wd < 5 else 0.35
    recency = 1.0 + 0.6 * max(0, (14 - (NOW.date() - d.date()).days)) / 14
    return base * recency * rnd.uniform(0.6, 1.4)


def claude_session(project, title, start, model, turns_n):
    cwd = path_of(project)
    sid = str(uuid.UUID(int=rnd.getrandbits(128)))
    d = os.path.join(HOME, '.claude', 'projects', cwd.replace('/', '-').replace('.', '-').replace('_', '-'))
    os.makedirs(d, exist_ok=True)
    lines = []
    t = start
    parent = None
    branch = 'main'
    for i in range(turns_n):
        uid = str(uuid.UUID(int=rnd.getrandbits(128)))
        text = title if i == 0 else rnd.choice(['继续', '再跑一遍测试', '把这段也改成异步', '看一下 CI 报错', '可以，提交吧'])
        lines.append({'parentUuid': parent, 'isSidechain': False, 'userType': 'external', 'cwd': cwd, 'sessionId': sid,
                      'version': '2.1.42', 'gitBranch': branch, 'type': 'user',
                      'message': {'role': 'user', 'content': text}, 'uuid': uid, 'timestamp': iso(t)})
        parent = uid
        for k in range(rnd.randint(2, 6)):
            t += timedelta(seconds=rnd.randint(4, 40))
            aid = str(uuid.UUID(int=rnd.getrandbits(128)))
            mid = 'msg_' + uuid.UUID(int=rnd.getrandbits(128)).hex[:24]
            tool = rnd.random() < 0.6
            content = ([{'type': 'tool_use', 'id': 'toolu_' + uuid.UUID(int=rnd.getrandbits(128)).hex[:24],
                         'name': rnd.choice(['Read', 'Edit', 'Bash', 'Grep', 'Write']),
                         'input': {'command': rnd.choice(['cargo test', 'npm test', 'git status', 'cargo clippy'])}}]
                       if tool else [{'type': 'text', 'text': '好的，我来处理。'}])
            usage = {'input_tokens': rnd.randint(4, 900), 'output_tokens': rnd.randint(80, 2600),
                     'cache_read_input_tokens': rnd.randint(8000, 90000),
                     'cache_creation_input_tokens': rnd.randint(0, 9000)}
            lines.append({'parentUuid': parent, 'isSidechain': False, 'userType': 'external', 'cwd': cwd, 'sessionId': sid,
                          'version': '2.1.42', 'gitBranch': branch,
                          'message': {'id': mid, 'type': 'message', 'role': 'assistant', 'model': model,
                                      'content': content, 'stop_reason': 'tool_use' if tool else 'end_turn',
                                      'usage': usage},
                          'requestId': 'req_' + uuid.UUID(int=rnd.getrandbits(128)).hex[:24],
                          'type': 'assistant', 'uuid': aid, 'timestamp': iso(t)})
            parent = aid
        t += timedelta(minutes=rnd.randint(1, 12))
    path = os.path.join(d, sid + '.jsonl')
    with open(path, 'w') as f:
        for l in lines:
            f.write(json.dumps(l, ensure_ascii=False) + '\n')
    ts = t.timestamp()
    os.utime(path, (ts, ts))


def codex_session(project, title, start, turns_n):
    cwd = path_of(project)
    sid = str(uuid.UUID(int=rnd.getrandbits(128)))
    d = os.path.join(HOME, '.codex', 'sessions', start.strftime('%Y'), start.strftime('%m'), start.strftime('%d'))
    os.makedirs(d, exist_ok=True)
    t = start
    lines = [
        {'timestamp': iso(t), 'type': 'session_meta',
         'payload': {'id': sid, 'timestamp': iso(t), 'cwd': cwd, 'originator': 'codex_cli_rs', 'cli_version': '0.50.0',
                     'model_provider': 'openai'}},
        {'timestamp': iso(t), 'type': 'turn_context', 'payload': {'cwd': cwd, 'model': 'gpt-5-codex'}},
    ]
    for i in range(turns_n):
        t += timedelta(seconds=rnd.randint(5, 60))
        msg = title if i == 0 else '继续'
        lines.append({'timestamp': iso(t), 'type': 'response_item',
                      'payload': {'type': 'message', 'role': 'user', 'content': [{'type': 'input_text', 'text': msg}]}})
        lines.append({'timestamp': iso(t), 'type': 'event_msg', 'payload': {'type': 'user_message', 'message': msg}})
        for k in range(rnd.randint(2, 5)):
            t += timedelta(seconds=rnd.randint(5, 50))
            lines.append({'timestamp': iso(t), 'type': 'response_item',
                          'payload': {'type': 'function_call', 'name': 'shell', 'call_id': 'call_' + uuid.UUID(int=rnd.getrandbits(128)).hex[:20],
                                      'arguments': json.dumps({'command': ['bash', '-lc', 'npm test']})}})
            lines.append({'timestamp': iso(t), 'type': 'event_msg', 'payload': {'type': 'token_count', 'info': {
                'last_token_usage': {'input_tokens': rnd.randint(9000, 60000), 'cached_input_tokens': rnd.randint(4000, 50000),
                                     'output_tokens': rnd.randint(200, 3000), 'reasoning_output_tokens': rnd.randint(0, 1500),
                                     'total_tokens': 0}}}})
    path = os.path.join(d, f"rollout-{start.strftime('%Y-%m-%dT%H-%M-%S')}-{sid}.jsonl")
    with open(path, 'w') as f:
        for l in lines:
            f.write(json.dumps(l, ensure_ascii=False) + '\n')
    ts = t.timestamp()
    os.utime(path, (ts, ts))


def write_sessions():
    for sub in ('.claude', '.codex'):
        p = os.path.join(HOME, sub)
        if os.path.exists(p):
            shutil.rmtree(p)
    projects = list(CLAUDE_TOPICS)
    weights = [5, 3, 3, 1, 1, 1]
    for back in range(30, -1, -1):
        day = (NOW - timedelta(days=back)).replace(hour=0, minute=0, second=0, microsecond=0)
        n = int(round(day_weight(day) * 4))
        for _ in range(n):
            proj = rnd.choices(projects, weights)[0]
            start = day + timedelta(hours=rnd.randint(9, 22), minutes=rnd.randint(0, 59))
            if start > NOW:
                continue
            claude_session(proj, rnd.choice(CLAUDE_TOPICS[proj]), start, pick_model(), rnd.randint(1, 4))
        if rnd.random() < 0.55:
            proj = rnd.choice(list(CODEX_TOPICS))
            start = day + timedelta(hours=rnd.randint(10, 21), minutes=rnd.randint(0, 59))
            if start <= NOW:
                codex_session(proj, rnd.choice(CODEX_TOPICS[proj]), start, rnd.randint(1, 3))
    # 今天上午 orbit-api 上的几条,保证 AI 历史面板顶上是眼熟的标题
    for i, title in enumerate(['修复订单查询的 N+1 问题', '给订单接口加限流中间件', '订单状态机重构']):
        claude_session('orbit-api', title, NOW - timedelta(hours=3 - i, minutes=17 * i), 'claude-opus-5-5', 3)


def write_home():
    # 演示 shell 的 rc、模拟 CLI、浅色/深色标记(模拟 CLI 据此挑 diff 配色)
    here = os.path.dirname(os.path.abspath(__file__))
    for rc in ('.bashrc', '.profile'):
        shutil.copy(os.path.join(here, 'home', 'bashrc'), os.path.join(HOME, rc))
    bin_dir = os.path.join(HOME, '.local', 'bin')
    os.makedirs(bin_dir, exist_ok=True)
    for name in ('claude', 'codex'):
        dst = os.path.join(bin_dir, name)
        shutil.copy(os.path.join(here, 'bin', name), dst)
        os.chmod(dst, 0o755)
    with open(os.path.join(HOME, '.demo-theme'), 'w') as f:
        f.write('light' if THEME == 'morning-mist' else 'dark')


if __name__ == '__main__':
    reset_dir(DATA)
    write_home()
    write_config()
    write_layout()
    write_themes()
    write_pricing()
    write_sessions()
    n_claude = sum(len(fs) for _, _, fs in os.walk(os.path.join(HOME, '.claude')))
    n_codex = sum(len(fs) for _, _, fs in os.walk(os.path.join(HOME, '.codex')))
    print(f'seeded {DATA} (theme={THEME}, hook={HOOK}); claude sessions={n_claude}, codex sessions={n_codex}')
