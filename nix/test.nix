# NixOS VM test of the module: the hardened unit binds port 25, takes its
# secrets through LoadCredential=, and forwards both anonymous and
# authenticated mail to a mock Bot API. Every secret here is a fake.
{ pkgs, self }:

let
  mockTelegram = pkgs.writeText "mock-telegram.py" ''
    import http.server, json, urllib.parse

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            fields = dict(urllib.parse.parse_qsl(body.decode()))
            with open("/var/lib/mock-telegram/requests.jsonl", "a") as log:
                log.write(json.dumps({"path": self.path, "fields": fields}) + "\n")
            reply = json.dumps({"ok": True, "result": {"message_id": 1}}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(reply)))
            self.end_headers()
            self.wfile.write(reply)

    http.server.HTTPServer(("127.0.0.1", 8080), Handler).serve_forever()
  '';
in
pkgs.testers.runNixOSTest {
  name = "smtp-to-telegram";

  nodes.machine = {
    imports = [ self.nixosModules.default ];

    environment.etc."smtp-to-telegram-test/bot-token".text = "42:FAKE-TOKEN";
    environment.etc."smtp-to-telegram-test/credentials".text = "router:correct horse\n";
    environment.systemPackages = [ pkgs.swaks ];

    systemd.services.mock-telegram = {
      wantedBy = [ "multi-user.target" ];
      before = [ "smtp-to-telegram.service" ];
      serviceConfig = {
        ExecStart = "${pkgs.python3}/bin/python3 ${mockTelegram}";
        StateDirectory = "mock-telegram";
      };
    };

    services.smtp-to-telegram = {
      enable = true;
      listen = [ "0.0.0.0:25" ];
      hostname = "relay.test";
      allowAnonymous = true;
      credentialsFile = "/etc/smtp-to-telegram-test/credentials";
      botTokenFile = "/etc/smtp-to-telegram-test/bot-token";
      chatIds = [ "4242" ];
      parseMode = "MarkdownV2";
      messageTemplate = "*{subject}*\\n{from}\\n\\n{body}";
      extraArgs = [
        "--telegram-api-prefix"
        "http://127.0.0.1:8080/"
      ];
    };
  };

  testScript = ''
    import json
    import re

    machine.wait_for_unit("mock-telegram.service")
    machine.wait_for_open_port(8080)
    machine.wait_for_unit("smtp-to-telegram.service")
    machine.wait_for_open_port(25)

    def requests():
        out = machine.succeed("cat /var/lib/mock-telegram/requests.jsonl || true")
        return [json.loads(line) for line in out.splitlines() if line]

    def capture(pattern: str, text: str) -> str:
        match = re.search(pattern, text, re.M)
        assert match is not None, f"{pattern!r} not found in:\n{text}"
        return match.group(1)

    swaks = "swaks --server 127.0.0.1:25 --ehlo client.test --from router@test --to alerts@test"

    with subtest("anonymous delivery"):
        machine.succeed(f"{swaks} --header 'Subject: anonymous alert (1)' --body 'link down'")
        last = requests()[-1]
        assert last["path"] == "/bot42:FAKE-TOKEN/sendMessage", last
        assert last["fields"]["chat_id"] == "4242", last
        assert last["fields"]["parse_mode"] == "MarkdownV2", last
        assert last["fields"]["text"] == "*anonymous alert \\(1\\)*\nrouter@test\n\nlink down", last

    with subtest("authenticated delivery"):
        for mechanism in ["PLAIN", "LOGIN"]:
            machine.succeed(
                f"{swaks} --auth {mechanism} --auth-user router --auth-password 'correct horse'"
                f" --header 'Subject: authenticated {mechanism}' --body ok"
            )
            assert requests()[-1]["fields"]["text"].startswith(f"*authenticated {mechanism}*"), requests()[-1]

    with subtest("wrong password is refused"):
        before = len(requests())
        out = machine.fail(
            f"{swaks} --auth PLAIN --auth-user router --auth-password wrong --header 'Subject: nope' 2>&1"
        )
        assert "535" in out, out
        assert len(requests()) == before

    with subtest("the process is unprivileged and its secrets are not on the command line"):
        pid = machine.succeed("systemctl show -p MainPID --value smtp-to-telegram.service").strip()
        status = machine.succeed(f"cat /proc/{pid}/status")
        uid = capture(r"^Uid:\s+(\d+)", status)
        assert uid != "0", status
        # CAP_NET_BIND_SERVICE is capability 10; nothing else may be set.
        for field in ["CapEff", "CapPrm", "CapBnd", "CapAmb"]:
            value = capture(rf"^{field}:\s+([0-9a-f]+)", status)
            assert int(value, 16) == 1 << 10, f"{field}={value}"
        cmdline = machine.succeed(f"tr '\\0' ' ' < /proc/{pid}/cmdline")
        assert "FAKE-TOKEN" not in cmdline and "correct horse" not in cmdline, cmdline
        # The credential exists for the service, and other users cannot read it.
        machine.succeed("test -s /run/credentials/smtp-to-telegram.service/telegram-bot-token")
        machine.fail("runuser -u nobody -- cat /run/credentials/smtp-to-telegram.service/telegram-bot-token")

    with subtest("systemd-analyze security rates the unit OK"):
        out = machine.succeed("systemd-analyze security --no-pager smtp-to-telegram.service")
        print(out)
        score = float(capture(r"Overall exposure level for smtp-to-telegram\.service: ([0-9.]+)", out))
        assert score <= 2.0, f"exposure {score}"
  '';
}
