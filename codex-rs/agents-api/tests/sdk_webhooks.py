"""Webhook endpoint coverage, called from the pinned SDK lifecycle test."""

import json
import threading
import typing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import openai
from openai.types.webhooks import ResponseCompletedWebhookEvent
from openai.types.webhooks.webhook_create_params import WebhookCreateParams

from sdk_sessions import rejected


def receiver():
    """A local receiver that records each delivery and answers 204."""
    deliveries = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            body = self.rfile.read(int(self.headers["content-length"]))
            deliveries.append((body, dict(self.headers)))
            self.send_response(204)
            self.end_headers()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, deliveries


def sdk_event_types():
    """The event types the pinned SDK accepts when creating an endpoint."""
    listed = typing.get_type_hints(WebhookCreateParams)["event_types"]
    while typing.get_origin(listed) is not typing.Literal:
        (listed,) = typing.get_args(listed)
    return list(typing.get_args(listed))


def check_webhooks(client):
    webhooks = client.webhooks
    assert webhooks.event_types.list().data == [
        "agent.session.created",
        "agent.session.action_required",
        "agent.session.in_progress",
        "agent.session.idle",
        "agent.session.failed",
        *sdk_event_types(),
    ]
    server, deliveries = receiver()
    url = f"http://127.0.0.1:{server.server_address[1]}/hook"
    try:
        created = webhooks.create(
            name="sdk-hooks", url=url, event_types=["agent.session.idle"]
        )
        assert created.signing_secret.startswith("whsec_")
        endpoint = webhooks.retrieve(created.id)
        assert endpoint.model_dump() == created.model_dump(exclude={"signing_secret"})
        assert created.id in [item.id for item in webhooks.list()]

        result = webhooks.test(created.id, event_type="agent.session.idle")
        assert result.model_dump() == {
            "event_type": "agent.session.idle",
            "object": "webhook_endpoint.test",
            "status_code": 204,
            "success": True,
            "webhook_endpoint_id": created.id,
        }
        body, headers = deliveries[-1]
        webhooks.verify_signature(body, headers, secret=created.signing_secret)
        event = json.loads(body)
        assert event["type"] == "agent.session.idle" and event["object"] == "event"

        # Event types this service never produces can be subscribed to, and
        # their test deliveries parse as the SDK's event models.
        updated = webhooks.update(created.id, event_types=sdk_event_types())
        assert updated.event_types == sdk_event_types()
        webhooks.test(created.id, event_type="response.completed")
        body, headers = deliveries[-1]
        event = webhooks.unwrap(body, headers, secret=created.signing_secret)
        assert isinstance(event, ResponseCompletedWebhookEvent), event

        rotated = webhooks.rotate_secret(
            created.id, keep_old_secret_active_for_24_hours=True
        )
        assert rotated.signing_secret != created.signing_secret
        webhooks.test(created.id, event_type="agent.session.created")
        body, headers = deliveries[-1]
        for secret in (created.signing_secret, rotated.signing_secret):
            webhooks.verify_signature(body, headers, secret=secret)

        updated = webhooks.update(created.id, name="sdk-hooks-renamed")
        assert updated.name == "sdk-hooks-renamed"
        for params in [
            {"event_types": ["response.created"]},
            {"url": "https://10.0.0.1/hook"},
        ]:
            rejected(openai.BadRequestError, webhooks.update, created.id, **params)
        assert webhooks.delete(created.id).model_dump() == {
            "id": created.id,
            "deleted": True,
            "object": "webhook_endpoint.deleted",
        }
        rejected(openai.NotFoundError, webhooks.retrieve, created.id)
    finally:
        server.shutdown()
