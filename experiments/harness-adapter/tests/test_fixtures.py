import json
import urllib.request


def _post(url: str, body: dict, key: str = "k") -> tuple[int, bytes]:
    req = urllib.request.Request(
        url,
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {key}"},
    )
    with urllib.request.urlopen(req) as resp:
        return resp.status, resp.read()


def test_fake_server_records_requests_and_replies(fake_chat_server):
    fake_chat_server.script(
        [{"tool_calls": [{"name": "ls", "arguments": {"path": "."}}]}, {"text": "ok"}]
    )
    url = f"{fake_chat_server.base_url}/chat/completions"

    _, raw = _post(url, {"model": "m", "messages": []}, key="sess-1")
    message = json.loads(raw)["choices"][0]["message"]
    assert message["tool_calls"][0]["function"]["name"] == "ls"

    _, raw = _post(url, {"model": "m", "messages": [], "stream": True})
    assert b"data: [DONE]" in raw and b'"content": "ok"' in raw

    assert [r.path for r in fake_chat_server.requests] == ["/v1/chat/completions"] * 2
    assert fake_chat_server.requests[0].headers["authorization"] == "Bearer sess-1"


def test_headless_binary_and_plugin_exist(headless_bin, openai_plugin, qmt_home):
    assert headless_bin.is_file()
    assert openai_plugin.suffix == ".wasm" and openai_plugin.is_file()
    assert str(openai_plugin) in (qmt_home / "providers.toml").read_text()
