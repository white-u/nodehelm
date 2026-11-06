#!/usr/bin/env python3
"""NodeHelm node menu: only what has to happen on the node itself.

Onboarding, configuration and removal stay in the panel, which confirms and
tracks them. Every action here calls the component's own tools.
"""
import json
import pathlib
import platform
import subprocess
import sys

CODE = pathlib.Path(__file__).resolve().parent
ROOT = pathlib.Path('/var/lib/node-manager')
AGENT = pathlib.Path('/opt/monitor/monitor-agent')
NAMES = {'snell': 'Snell', 'sb': 'SB', 'fw': 'FW', 'pm': 'PM'}
ENV = {'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C.UTF-8', 'PYTHONDONTWRITEBYTECODE': '1'}


def run(*command, check=True):
    code = subprocess.run(command, env=ENV, check=False).returncode
    if check and code:
        raise SystemExit(code)
    return code


def tool(name, *args, check=True):
    return run(sys.executable, '-B', str(CODE / name), *args, check=check)


def active(unit):
    result = subprocess.run(('systemctl', 'is-active', unit), capture_output=True, text=True, env=ENV, check=False)
    return result.stdout.strip() or 'unknown'


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
        print('已是后台提供的版本')
        return
    # The same command the panel's update page gives, from the hub this node already trusts.
    script = subprocess.run(('curl', '-fsSL', '--proto', '=https', hub + '/install.sh'),
                            capture_output=True, env=ENV, check=False)
    if script.returncode:
        raise SystemExit('下载监控 Agent 安装脚本失败')
    code = subprocess.run(('sh', '-s', '--', '--upgrade'), input=script.stdout, env=ENV, check=False).returncode
    if code:
        raise SystemExit(code)


def status():
    hub = server()
    print(f'管理组件: {active("node-manager-agent.service")}' + (f'（后台 {hub}）' if hub else '（未绑定）'))
    if has_unit('monitor-agent.service'):
        print(f'监控 Agent: {active("monitor-agent.service")}')
    for journal in ('component-recovery.json', 'lifecycle-job.json'):
        if (ROOT / journal).exists():
            print(f'待处理记录: {journal}，可选择「恢复中断的操作」或回到后台继续')
    result = subprocess.run((sys.executable, '-B', str(CODE / 'agent.py'), 'status'),
                            capture_output=True, text=True, env=ENV, check=False)
    try:
        state = json.loads(result.stdout)
    except ValueError:
        print('模块状态读取失败：' + (result.stderr.strip().splitlines() or ['未知错误'])[-1])
        return
    for name, module in [('snell', state), *state.get('modules', {}).items()]:
        if name not in NAMES:
            continue
        count = len(module.get('resources', []))
        line = f'{NAMES[name]}: ' + ('已安装' if module.get('installed') else '未安装')
        if count:
            line += f'，{count} 项配置'
        if module.get('blocked'):
            line += '，有待处理事务'
        print(line)


def logs():
    run('journalctl', '-u', 'node-manager-agent', '-n', '50', '--no-pager', check=False)


def recover():
    # A component switch first, then a business transaction; each says when there is nothing to do.
    tool('install.py', 'recover')
    tool('agent.py', 'recover')


def removal():
    print('''卸载请在后台操作：服务管理 → 本节点 →「管理」页 →「全部卸载此节点」。
它清理全部受管实例、核心、PM 规则和管理组件（包括本命令），进度可在后台查看和继续。
后台已无法使用时的本机兜底流程，见部署文档「节点卸载」一节。''')


ACTIONS = {'upgrade': upgrade, 'status': status, 'logs': logs, 'recover': recover, 'uninstall': removal}
MENU = [('升级管理组件和监控 Agent', 'upgrade'), ('运行状态', 'status'), ('最近日志', 'logs'),
        ('恢复中断的操作', 'recover'), ('卸载说明', 'uninstall')]


def menu():
    print('NodeHelm 节点：')
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
    import os
    import signal
    # Piped into head or grep, stop quietly like any other command.
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    # Headings must come before the output of the tools they introduce.
    sys.stdout.reconfigure(line_buffering=True)
    if os.geteuid() != 0:
        raise SystemExit('请以 root 运行：sudo nodehelm')
    os.umask(0o077)
    if len(argv) > 1 or (argv and argv[0] not in ACTIONS):
        raise SystemExit('用法: nodehelm [' + '|'.join(ACTIONS) + ']')
    if argv:
        ACTIONS[argv[0]]()
    elif sys.stdin.isatty():
        menu()
    else:
        status()


if __name__ == '__main__':
    main(sys.argv[1:])
