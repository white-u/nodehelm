"""Root-private durable files. Replace + fsync file and parent before effects."""
import json
import os
import pathlib
import tempfile
from model import canonical


def private_directory(path):
    path = pathlib.Path(path)
    if path.is_symlink():
        raise ValueError('拒绝符号链接目录')
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    if path.stat().st_uid != os.geteuid() or path.stat().st_mode & 0o077:
        raise ValueError('管理目录须归当前用户所有且权限为 0700')
    return path


def read(path, default=None):
    path = pathlib.Path(path)
    if path.is_symlink():
        raise ValueError('拒绝符号链接文件')
    try:
        with path.open(encoding='utf-8') as f:
            return json.load(f)
    except FileNotFoundError:
        return default


def write(path, value):
    path = pathlib.Path(path)
    if path.is_symlink():
        raise ValueError('拒绝符号链接文件')
    fd, tmp = tempfile.mkstemp(prefix='.new-', dir=path.parent)
    try:
        with os.fdopen(fd, 'w', encoding='utf-8') as f:
            f.write(canonical(value))
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
        sync(path.parent)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def sync(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def remove(path):
    path = pathlib.Path(path)
    path.unlink(missing_ok=True)
    sync(path.parent)
