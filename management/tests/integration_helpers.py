"""Fixture preparation for Linux systemd integration tests."""
import pathlib
import shutil


def stage_snell_launcher(directory):
    # The unit runs snell.py beside the core; its launch path needs nothing else.
    target = directory / 'snell.py'
    shutil.copyfile(pathlib.Path(__file__).resolve().parents[1] / 'snell.py', target)
    target.chmod(0o644)
