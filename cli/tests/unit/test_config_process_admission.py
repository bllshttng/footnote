"""Process admission uses a process-valued config block, never agent capacity."""

import fno.config as config


def _block_type():
    block = getattr(config, "ProcessAdmissionBlock", None)
    assert block is not None, "ProcessAdmissionBlock must own the process-unit ceiling"
    return block


def test_process_admission_default_and_agents_max_live_are_independent():
    block = _block_type()
    settings = config.ConfigBlock(
        agents={"max_live": 30},
        process_admission={"max_processes": 650},
    )

    assert block().max_processes == 400
    assert settings.agents.max_live == 30
    assert settings.process_admission.max_processes == 650


def test_invalid_process_ceiling_coerces_to_process_default():
    block = _block_type()

    for value in (0, -1, True, "many", None):
        assert block(max_processes=value).max_processes == 400
