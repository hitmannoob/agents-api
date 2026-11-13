"""Shared fixtures and client setup for the pinned Agents API SDK checks."""

import copy
import importlib
import json
from functools import wraps
from pathlib import Path

import openai


SDK_VERSION = "3.17.0"
TOKEN = "test-token-for-the-local-agents-api"
FIXTURES = Path(__file__).with_name("fixtures") / "sdk_contract.json"
INVENTORY = Path(__file__).resolve().parent.parent / "CONTRACT_INVENTORY.json"
_SDK_CALLS = set()
_SDK_METHODS_WRAPPED = set()


def require_pinned_sdk():
    assert openai.__version__ == SDK_VERSION, openai.__version__


def fixture(name):
    with FIXTURES.open(encoding="utf-8") as handle:
        return copy.deepcopy(json.load(handle)[name])


def _operations():
    with INVENTORY.open(encoding="utf-8") as handle:
        return json.load(handle)["operations"]


def _record_method(method, operation):
    @wraps(method)
    def recorded(*args, **kwargs):
        # Expected HTTP errors count too: the scenario asserts their contract.
        _SDK_CALLS.add(operation)
        return method(*args, **kwargs)

    return recorded


def _wrap_sdk_methods():
    for row in _operations():
        resource_name, method_name = row.get("sdk_resource"), row.get("sdk_method")
        if not resource_name or not method_name:
            continue
        operation = f"{resource_name}.{method_name}"
        if operation in _SDK_METHODS_WRAPPED:
            continue
        module_name, class_name = resource_name.split(":", 1)
        resource = getattr(importlib.import_module(module_name), class_name)
        method = getattr(resource, method_name)
        setattr(
            resource,
            method_name,
            _record_method(method, operation),
        )
        _SDK_METHODS_WRAPPED.add(operation)


def assert_scenario_coverage(script_name):
    expected = [
        row
        for row in _operations()
        if row["status"] != "missing" and row.get("sdk_scenario") == script_name
    ]
    assert expected, f"no SDK operations assigned to {script_name}"
    uncalled = [
        f"{row['id']}: {row['sdk_resource']}.{row['sdk_method']}"
        for row in expected
        if f"{row['sdk_resource']}.{row['sdk_method']}" not in _SDK_CALLS
    ]
    assert not uncalled, f"{script_name}: uncalled SDK operations:\n" + "\n".join(uncalled)


def client(base_url):
    require_pinned_sdk()
    # Wrap classes before the client can bind methods for raw/streaming helpers.
    _wrap_sdk_methods()
    return openai.OpenAI(
        base_url=base_url,
        api_key=TOKEN,
        max_retries=0,
        timeout=20,
        _strict_response_validation=True,
    )


def message(text):
    event = fixture("message_event")
    event["input"][0]["content"][0]["text"] = text
    return event
