"""Versioned, deliberately small management protocol. No shell commands or paths.

Shared by every module: the request envelope, revisions, errors and the field
checks more than one module uses. What a module accepts lives in its own file
and is reached through `modules.py`."""
from __future__ import annotations
import hashlib
import ipaddress
import json
import re

# What this component and the Hub must agree on; see PROTOCOL in src/management/mod.rs.
PROTOCOL = 1
CORE_ACTIONS = ('core_upgrade', 'core_rollback', 'core_uninstall')
ACTIONS = ('install', 'apply', 'delete', 'check', 'reset', *CORE_ACTIONS)
# Actions only some modules take, refused before the revision is compared.
OPTIONAL = {'check': '目标检查仅适用于 FW', 'reset': '用量重置仅适用于 PM'}

class Rejected(ValueError):
    def __init__(self, message, code='rejected'):
        super().__init__(message)
        self.code = code

def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=True)

def revision(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()

def address(value):
    if not isinstance(value, str) or not value or len(value) > 253:
        raise Rejected('请填写有效的客户端访问域名或 IP')
    try:
        ipaddress.ip_address(value)
        return value
    except ValueError:
        if re.fullmatch(r'(?=.{1,253}$)(?:[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)*[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?', value):
            return value.lower()
    raise Rejected('访问地址不能包含协议、路径、端口或配置分隔符')

def fields(value, allowed):
    if not isinstance(value, dict) or set(value) - allowed:
        raise Rejected('配置包含不支持的字段')
    return value

def port(value, message='监听端口须为 1–65535'):
    if type(value) is not int or not 1 <= value <= 65535:
        raise Rejected(message)
    return value

def name(value):
    if not isinstance(value, str) or not re.fullmatch(r'[\w -]{1,48}', value, re.UNICODE):
        raise Rejected('名称仅支持文字、数字、空格、横线和下划线，最多 48 字符')
    return value

def request(value, modules):
    """`modules` maps a module name to what it accepts: `ACTIONS` and `resource`."""
    if not isinstance(value, dict) or set(value) - {'id', 'action', 'revision', 'payload', 'module', 'intent'} or not {'id', 'action', 'revision', 'payload'} <= set(value):
        raise Rejected('请求格式错误')
    if not isinstance(value['id'], str) or not re.fullmatch(r'[a-f0-9]{32}', value['id']):
        raise Rejected('操作编号无效')
    module = modules.get(value.get('module', 'snell')) if isinstance(value.get('module', 'snell'), str) else None
    if module is None:
        raise Rejected('不支持的管理模块')
    action = value['action']
    if action not in ACTIONS:
        raise Rejected('不支持的操作')
    if 'intent' in value and (action != 'apply' or value['intent'] not in ('create', 'update')):
        raise Rejected('配置操作意图无效')
    if action in OPTIONAL and action not in module.ACTIONS:
        raise Rejected(OPTIONAL[action])
    if not isinstance(value['revision'], str) or not re.fullmatch(r'[a-f0-9]{64}', value['revision']):
        raise Rejected('配置版本无效，请刷新')
    if action == 'apply':
        value = {**value, 'payload': module.resource(value['payload'])}
    elif action in ('delete', 'check', 'reset'):
        p = value['payload']
        if not isinstance(p, dict) or set(p) != {'port'} or type(p['port']) is not int or not 1 <= p['port'] <= 65535:
            raise Rejected('删除目标无效')
    elif action in ('core_upgrade', 'core_rollback'):
        p = value['payload']
        if (action not in module.ACTIONS or not isinstance(p, dict) or set(p) != {'version'}
                or not isinstance(p['version'], str)
                or not re.fullmatch(r'(?:0|[1-9][0-9]{0,4})(?:\.(?:0|[1-9][0-9]{0,4})){2}', p['version'])):
            raise Rejected('请选择受信任的核心版本', 'core_version')
    elif value['payload'] != {}:
        raise Rejected('此操作不接受自定义参数')
    return value
