"""Versioned, deliberately small management protocol. No shell commands or paths."""
from __future__ import annotations
import hashlib
import ipaddress
import json
import re

# What this component and the Hub must agree on; see PROTOCOL in src/management/mod.rs.
PROTOCOL = 1
CORE_ACTIONS = ('core_upgrade', 'core_rollback', 'core_uninstall')

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

def resource(value, module="snell"):
    if module != "snell":
        from modules_model import resource as other_resource
        return other_resource(value, module)
    if not isinstance(value, dict) or set(value) - {'port', 'name', 'host', 'public_port', 'psk', 'mode'}:
        raise Rejected('配置包含不支持的字段')
    port = value.get('port')
    public_port = value.get('public_port', port)
    if type(port) is not int or not 1 <= port <= 65535:
        raise Rejected('监听端口须为 1–65535')
    if type(public_port) is not int or not 1 <= public_port <= 65535:
        raise Rejected('访问端口须为 1–65535')
    name = value.get('name', '')
    if not isinstance(name, str) or not re.fullmatch(r'[\w -]{1,48}', name, re.UNICODE):
        raise Rejected('名称仅支持文字、数字、空格、横线和下划线，最多 48 字符')
    mode = value.get('mode', 'default')
    if mode not in ('default', 'unshaped'):
        raise Rejected('首版仅支持加密模式 default 和 unshaped')
    psk = value.get('psk', '')
    if not isinstance(psk, str) or (psk and not re.fullmatch(r'[A-Za-z0-9_-]{22,128}', psk)):
        raise Rejected('PSK 必须为 22–128 位字母、数字、横线或下划线；留空自动生成或保留')
    return dict(port=port, name=name, host=address(value.get('host')), public_port=public_port, mode=mode, psk=psk)

def request(value):
    if not isinstance(value, dict) or set(value) - {'id', 'action', 'revision', 'payload', 'module', 'intent'} or not {'id', 'action', 'revision', 'payload'} <= set(value):
        raise Rejected('请求格式错误')
    if not isinstance(value['id'], str) or not re.fullmatch(r'[a-f0-9]{32}', value['id']):
        raise Rejected('操作编号无效')
    if value.get('module', 'snell') not in ('snell', 'sb', 'fw', 'pm'):
        raise Rejected('不支持的管理模块')
    if value['action'] not in ('install', 'apply', 'delete', 'check', 'reset', *CORE_ACTIONS):
        raise Rejected('不支持的操作')
    if 'intent' in value and (value['action'] != 'apply' or value['intent'] not in ('create', 'update')):
        raise Rejected('配置操作意图无效')
    if value['action'] == 'check' and value.get('module') != 'fw':
        raise Rejected('目标检查仅适用于 FW')
    if value['action'] == 'reset' and value.get('module') != 'pm':
        raise Rejected('用量重置仅适用于 PM')
    if not isinstance(value['revision'], str) or not re.fullmatch(r'[a-f0-9]{64}', value['revision']):
        raise Rejected('配置版本无效，请刷新')
    if value['action'] == 'apply':
        value = {**value, 'payload': resource(value['payload'], value.get('module', 'snell'))}
    elif value['action'] in ('delete', 'check', 'reset'):
        p = value['payload']
        if not isinstance(p, dict) or set(p) != {'port'} or type(p['port']) is not int or not 1 <= p['port'] <= 65535:
            raise Rejected('删除目标无效')
    elif value['action'] in ('core_upgrade', 'core_rollback'):
        p = value['payload']
        if (value.get('module') == 'pm' or not isinstance(p, dict) or set(p) != {'version'}
                or not isinstance(p['version'], str)
                or not re.fullmatch(r'(?:0|[1-9][0-9]{0,4})(?:\.(?:0|[1-9][0-9]{0,4})){2}', p['version'])):
            raise Rejected('请选择受信任的核心版本', 'core_version')
    elif value['payload'] != {}:
        raise Rejected('此操作不接受自定义参数')
    return value
