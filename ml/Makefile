PYTHON ?= python
RUFF ?= ruff
MYPY ?= mypy
PYTEST ?= pytest
PIP_AUDIT ?= pip-audit
AUDIT_REQUIREMENTS ?= requirements.txt

.PHONY: models check
models:
	$(PYTHON) download.py .models
check:
	$(RUFF) check yhotos_ml tests download.py
	$(MYPY) --strict yhotos_ml download.py
	$(PYTEST)
	$(PIP_AUDIT) --cache-dir /tmp/yhotos-pip-audit --disable-pip --no-deps --require-hashes -r $(AUDIT_REQUIREMENTS)
