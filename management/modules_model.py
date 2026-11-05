"""Fixed field schemas for SB and FW. No user-selected executable or path."""
import base64
import datetime as dt
import ipaddress
import re
from model import Rejected, address


def resource(value, module):
    common = {'port', 'name'}
    if module == 'pm':
        fields = common | {'quota_gb', 'quota_mode', 'reset_day', 'expiry_date', 'egress_mbps'}
    else:
        fields = common | ({'host', 'public_port', 'protocol', 'sni', 'psk'} if module == 'sb' else {'target_host', 'target_port'})
    if not isinstance(value, dict) or set(value) - fields:
        raise Rejected('配置包含不支持的字段')
    port = value.get('port')
    if type(port) is not int or not 1 <= port <= 65535:
        raise Rejected('监听端口须为 1–65535')
    name = value.get('name', '')
    if not isinstance(name, str) or not re.fullmatch(r'[\w -]{1,48}', name):
        raise Rejected('名称仅支持文字、数字、空格、横线和下划线，最多 48 字符')
    if module == 'pm':
        quota, reset_day, rate = (value.get(key) for key in ('quota_gb', 'reset_day', 'egress_mbps'))
        mode, expiry = value.get('quota_mode'), value.get('expiry_date')
        if (port < 1024 or type(quota) is not int or not 0 <= quota <= 8589934591
                or type(reset_day) is not int or not 0 <= reset_day <= 31
                or type(rate) is not int or not 0 <= rate <= 10000
                or mode not in ('in_out', 'out_only') or not isinstance(expiry, str)):
            raise Rejected('PM 端口或策略值无效')
        if expiry:
            try:
                if dt.date.fromisoformat(expiry).isoformat() != expiry:
                    raise ValueError()
            except ValueError:
                raise Rejected('PM 到期日期须为 YYYY-MM-DD') from None
        return dict(port=port, name=name, quota_gb=quota, quota_mode=mode, reset_day=reset_day,
                    expiry_date=expiry, egress_mbps=rate)
    if module == 'fw':
        try: target = str(ipaddress.IPv4Address(value.get('target_host', '')))
        except (ValueError, TypeError): raise Rejected('目标必须为有效 IPv4 地址') from None
        ip = ipaddress.ip_address(target)
        if ip.is_unspecified or ip.is_multicast or ip.is_link_local:
            raise Rejected('目标不能为未指定、组播或链路本地地址')
        target_port = value.get('target_port', port)
        if type(target_port) is not int or not 1 <= target_port <= 65535:
            raise Rejected('目标端口须为 1–65535')
        return dict(port=port, name=name, target_host=target, target_port=target_port)
    protocol = value.get('protocol', 'reality')
    if protocol not in ('reality', 'ss2022'):
        raise Rejected('SB 仅支持 Reality 和 SS2022')
    public_port = value.get('public_port', port)
    if type(public_port) is not int or not 1 <= public_port <= 65535:
        raise Rejected('访问端口须为 1–65535')
    psk = value.get('psk', '')
    if not isinstance(psk, str): raise Rejected('密钥格式错误')
    if psk:
        try:
            if protocol != 'ss2022' or len(base64.b64decode(psk, validate=True)) != 16:
                raise ValueError()
        except ValueError: raise Rejected('SS2022 密钥须为 16 字节的标准 Base64；留空生成或保留') from None
    sni = value.get('sni', '')
    if protocol == 'reality':
        sni = address(sni)
        if '.' not in sni or ':' in sni or sni.replace('.', '').isdigit():
            raise Rejected('Reality 目标须为完整域名')
    else: sni = ''
    return dict(port=port, name=name, host=address(value.get('host')), public_port=public_port, protocol=protocol, sni=sni, psk=psk)
