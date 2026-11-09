"""The business modules this component runs. Each lives in its own file, with
`resource` (accepted fields), its `SPEC`, `Driver` and `controller(root)`; one
module's changes stay in its file. Rules spanning modules (PM following a
business port) sit in agent.py and lifecycle.py, not in any module. Adding a
module: see "四个业务模块入口" in AGENTS.md."""
import fw
import pm
import sb
import snell
from core_driver import CoreDriver
from protocol import request as parse

MODULES = {module.SPEC.name: module for module in (snell, sb, fw, pm)}
SPECS = {name: module.SPEC for name, module in MODULES.items()}


def request(value):
    return parse(value, SPECS)


def controllers(root):
    return {name: module.controller(root) for name, module in MODULES.items()}


def core_drivers(root, code):
    """The drivers that own a downloaded core, keyed by its file name."""
    drivers = (module.Driver(root, code) for module in MODULES.values() if issubclass(module.Driver, CoreDriver))
    return {driver.binary.name: driver for driver in drivers}
