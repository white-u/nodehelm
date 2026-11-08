"""The business modules this component runs. Each lives in its own file, with
`MODULE`, `ACTIONS`, `resource` (accepted fields) and `controller(root)`; one
module's changes stay in its file. Adding a module: its file, an entry here and
in the Hub's `FILES`. Rules spanning modules (PM following a business port) sit
in agent.py and lifecycle.py, not in any module."""
import fw
import pm
import sb
import snell
from core_driver import CoreDriver
from protocol import request as parse

MODULES = {module.MODULE: module for module in (snell, sb, fw, pm)}


def request(value):
    return parse(value, MODULES)


def controllers(root):
    return {name: module.controller(root) for name, module in MODULES.items()}


def core_drivers(root, code):
    """The drivers that own a downloaded core, keyed by its file name."""
    drivers = (module.Driver(root, code) for module in MODULES.values() if issubclass(module.Driver, CoreDriver))
    return {driver.binary.name: driver for driver in drivers}
