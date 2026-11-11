#!/usr/bin/env python3
"""NodeHelm node menu: only what has to happen on the node itself.

Onboarding and configuration stay in the panel. Every action here calls the
component's own tools.
"""
import fcntl
import json
import os
import pathlib
import platform
import subprocess
import sys

CODE = pathlib.Path(__file__).resolve().parent
ROOT = pathlib.Path('/var/lib/node-manager')
AGENT = pathlib.Path('/opt/monitor/monitor-agent')
MODULES = ('snell', 'sb', 'fw', 'pm')
ENV = {'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C.UTF-8', 'PYTHONDONTWRITEBYTECODE': '1'}
# What an interrupted component switch and an interrupted module operation leave behind.
COMPONENT_JOURNAL = 'component-recovery.json'
MODULE_JOURNALS = ('recovery.json', 'nft-transition.json')


def run(*command, check=True):
    code = subprocess.run(command, env=ENV, check=False).returncode
    if check and code:
        raise SystemExit(code)
    return code


def tool(name, *args, check=True):
    return run(sys.executable, '-B', str(CODE / name), *args, check=check)


def active(unit):
    result = subprocess.run(('systemctl', 'is-active', unit), capture_output=True, text=True, env=ENV, check=False)
    state = result.stdout.strip() or 'unknown'
    return '运行中' if state == 'active' else f'未运行（{state}）'


def has_unit(unit):
    return subprocess.run(('systemctl', 'cat', unit), capture_output=True, env=ENV, check=False).returncode == 0


def server():
    try:
        return json.loads((ROOT / 'binding.json').read_text())['server']
    except (OSError, ValueError, KeyError):
        return ''


def agent_current(hub):
    """Whether the probe already runs what the hub serves; reinstalling it would only restart it."""
    arch = {'x86_64': 'x86_64', 'amd64': 'x86_64', 'aarch64': 'aarch64', 'arm64': 'aarch64'}.get(platform.machine())
    if not arch or not AGENT.is_file():
        return False
    served = subprocess.run(('curl', '-fsSL', '--proto', '=https', '--max-time', '300', f'{hub}/agent/{arch}'),
                            capture_output=True, env=ENV, check=False)
    return served.returncode == 0 and served.stdout == AGENT.read_bytes()


def upgrade():
    print('== 管理组件')
    tool('install.py', 'upgrade')
    if not has_unit('monitor-agent.service'):
        return
    hub = server()
    if not hub:
        print('未绑定后台，跳过监控 Agent')
        return
    print('== 监控 Agent')
    if agent_current(hub):
        print('已是最新')
        return
    # The same command the panel's update page gives, from the hub this node already trusts.
    script = subprocess.run(('curl', '-fsSL', '--proto', '=https', hub + '/install.sh'),
                            capture_output=True, env=ENV, check=False)
    if script.returncode:
        raise SystemExit('下载监控 Agent 安装脚本失败')
    code = subprocess.run(('sh', '-s', '--', '--upgrade'), input=script.stdout, env=ENV, check=False).returncode
    if code:
        raise SystemExit(code)


def logs():
    hub = server()
    print(f'管理组件: {active("node-manager-agent.service")}' + (f'（后台 {hub}）' if hub else '（未绑定）'))
    if has_unit('monitor-agent.service'):
        print(f'监控 Agent: {active("monitor-agent.service")}')
    run('journalctl', '-u', 'node-manager-agent', '-n', '50', '--no-pager', check=False)


def interrupted():
    """The component switch and module directories an interrupted operation left a record in."""
    modules = [ROOT if name == 'snell' else ROOT / name for name in MODULES]
    return ((ROOT / COMPONENT_JOURNAL).exists(),
            [directory for directory in modules if any((directory / name).exists() for name in MODULE_JOURNALS)])


def busy(directory):
    """A running operation holds its module's write lock for as long as its record exists."""
    try:
        fd = os.open(directory / 'write.lock', os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return False
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return False
    except BlockingIOError:
        return True
    finally:
        os.close(fd)


def finish(component, modules):
    """Completes what was interrupted, the component switch first; recovery stops the poller."""
    if any(busy(directory) for directory in modules):
        raise SystemExit('有操作正在进行，请稍后再试')
    print('完成上次中断的操作…')
    if component and tool('install.py', 'recover', check=False):
        raise SystemExit('后台不可达时运行 sudo python3 /opt/node-manager/install.py recover --offline')
    if modules:
        result = subprocess.run((sys.executable, '-B', str(CODE / 'agent.py'), 'recover'),
                                capture_output=True, text=True, env=ENV, check=False)
        print(result.stderr, end='', file=sys.stderr)
        if result.returncode:
            raise SystemExit(result.returncode)
        outcome = json.loads(result.stdout)
        if outcome.get('status') == 'recovery_failed':
            raise SystemExit('恢复失败：' + outcome.get('message', ''))
    print('已完成')


def rebind():
    server = ask('后台地址: ')
    agent = (sys.executable, '-B', str(CODE / 'agent.py'), 'rebind', '--server', server)
    first = subprocess.run(agent, input='', text=True, capture_output=True, env=ENV, check=False)
    if first.returncode == 3:
        code = ask('接入码: ')
        answer = subprocess.run(agent, input=json.dumps({'enrollment': code}), text=True, env=ENV, check=False)
        raise SystemExit(answer.returncode)
    print(first.stdout, end='')
    print(first.stderr, end='', file=sys.stderr)
    raise SystemExit(first.returncode)


def uninstall():
    if ask('卸载本节点全部服务与管理组件？[y/N] ').lower() not in ('y', 'yes'):
        raise SystemExit('已取消')
    tool('agent.py', 'uninstall')


def ask(prompt):
    try:
        return input(prompt).strip()
    except EOFError:
        raise SystemExit('已取消') from None


ACTIONS = {'upgrade': upgrade, 'logs': logs, 'rebind': rebind, 'uninstall': uninstall}
MENU = [('升级', 'upgrade'), ('日志', 'logs'), ('换绑', 'rebind'), ('卸载', 'uninstall')]
USAGE = '用法: nodehelm [' + '|'.join(ACTIONS) + ']'


def menu():
    print('NodeHelm 节点')
    for index, (label, _) in enumerate(MENU, 1):
        print(f'  {index}) {label}')
    print('  0) 退出')
    try:
        choice = input('请选择 [1]: ').strip() or '1'
    except EOFError:
        return
    if choice == '0':
        return
    if not choice.isdigit() or not 1 <= int(choice) <= len(MENU):
        raise SystemExit('无效选择')
    ACTIONS[MENU[int(choice) - 1][1]]()


def main(argv):
    import signal
    # Piped into head or grep, stop quietly like any other command.
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    # Headings must come before the output of the tools they introduce.
    sys.stdout.reconfigure(line_buffering=True)
    if os.geteuid() != 0:
        raise SystemExit('请以 root 运行：sudo nodehelm')
    os.umask(0o077)
    if len(argv) > 1 or (argv and argv[0] not in ACTIONS):
        raise SystemExit(USAGE)
    if not argv and not sys.stdin.isatty():
        raise SystemExit(USAGE)
    # Anything interrupted blocks every other change, so it is finished first and
    # the run stops there: what was asked may no longer apply.
    component, modules = interrupted()
    if component or modules:
        if argv == ['logs']:
            print('有中断的操作，不带参数再运行即可完成')
        else:
            return finish(component, modules)
    if argv:
        ACTIONS[argv[0]]()
    else:
        menu()


if __name__ == '__main__':
    main(sys.argv[1:])
