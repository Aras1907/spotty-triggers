"""Spotty's in-app Proton VPN client.

Runs Proton's official Linux client library (proton-vpn-api-core, GPL-3.0)
from the bundle compiled into Spotty and speaks JSON lines with Spotty:

    request  {"id": 1, "cmd": "connect", "args": {"kind": "country", "value": "CH"}}
    reply    {"id": 1, "ok": true, "data": {...}}  or  {"id": 1, "ok": false, "error": "..."}
    event    {"event": "state", "data": {...}}

Passwords and 2FA codes arrive only over stdin and go straight to Proton's
library; nothing here stores them. Proton's library keeps the session in the
desktop keyring, exactly as Proton's own app does.

Usage: python3 -I spotty_vpn_helper.py <bundle dir> <cache dir>
"""
import asyncio
import json
import os
import sys
import threading

BUNDLE, CACHE = sys.argv[1], sys.argv[2]
sys.path[0:0] = [os.path.join(BUNDLE, "proton"), os.path.join(BUNDLE, "site")]

# stdout carries the protocol only; anything a library prints goes to stderr.
_PROTOCOL = sys.stdout
sys.stdout = sys.stderr
_WRITE = threading.Lock()


def emit(message):
    with _WRITE:
        _PROTOCOL.write(json.dumps(message) + "\n")
        _PROTOCOL.flush()


from proton.vpn.core.api import ProtonVPNAPI  # noqa: E402
from proton.vpn.core.session_holder import ClientTypeMetadata  # noqa: E402
from proton.session.exceptions import ProtonAPIAuthenticationNeeded  # noqa: E402

PROTOCOL_LABELS = {
    "wireguard": "WireGuard",
    "openvpn-udp": "OpenVPN (UDP)",
    "openvpn-tcp": "OpenVPN (TCP)",
}


class Client:
    def __init__(self):
        # Proton's library identifies itself as its Linux GUI client.
        self.api = ProtonVPNAPI(ClientTypeMetadata(type="gui"))
        self.connector = None
        self.refresher_enabled = False
        self.state_waiters = []

    # ── lifecycle ────────────────────────────────────────────────────────
    async def start(self):
        self.connector = await self.api.get_vpn_connector()
        self.connector.register(self)
        if self.api.is_user_logged_in():
            try:
                await self.enable_refresher()
            except ProtonAPIAuthenticationNeeded:
                await self.drop_expired_session()

    async def drop_expired_session(self):
        """A saved sign-in Proton no longer accepts: sign out locally, as
        Proton's own app does, so the user can sign in again."""
        print("spotty-vpn: saved Proton session expired; signing out", file=sys.stderr)
        self.refresher_enabled = False
        try:
            await self.api.logout()
        except Exception as error:  # pylint: disable=broad-except
            print(f"spotty-vpn: local sign-out failed: {error}", file=sys.stderr)
        emit({"event": "signed_out", "data": {}})

    async def enable_refresher(self):
        if self.refresher_enabled:
            return
        self.api.refresher.set_error_callback(
            lambda error: emit({"event": "error", "data": str(error)}))
        await self.api.refresher.enable()
        self.refresher_enabled = True
        self.write_countries()

    async def disable_refresher(self):
        if self.refresher_enabled:
            await self.api.refresher.disable()
            self.api.refresher.unset_error_callback()
            self.refresher_enabled = False

    # ── connection state ─────────────────────────────────────────────────
    def status_update(self, state):  # Proton's VPNStateSubscriber hook
        data = self.state_data(state)
        emit({"event": "state", "data": data})
        for waiter in list(self.state_waiters):
            if not waiter.done() and data["state"] in ("Connected", "Disconnected", "Error"):
                waiter.set_result(data)

    def state_data(self, state=None):
        state = state or (self.connector.current_state if self.connector else None)
        name = type(state).__name__ if state else "Disconnected"
        server = country = ""
        connection = getattr(getattr(state, "context", None), "connection", None)
        if connection is not None:
            server = getattr(connection, "server_name", "") or ""
            try:
                country = self.api.server_list.get_by_name(server).exit_country
            except Exception:  # pylint: disable=broad-except
                country = ""
        error = ""
        if name == "Error":
            event = getattr(getattr(state, "context", None), "event", None)
            error = type(event).__name__ if event else "Connection failed"
        return {"state": name, "server": server, "country": country, "error": error}

    async def wait_for_result(self, timeout):
        loop = asyncio.get_running_loop()
        waiter = loop.create_future()
        self.state_waiters.append(waiter)
        try:
            return await asyncio.wait_for(waiter, timeout)
        except asyncio.TimeoutError:
            return self.state_data()
        finally:
            self.state_waiters.remove(waiter)

    # ── commands ─────────────────────────────────────────────────────────
    async def cmd_status(self, _args):
        logged_in = self.api.is_user_logged_in()
        account = plan = ""
        if logged_in:
            account = self.api.account_name or ""
            try:
                plan = self.api.account_data.plan_title or ""
            except Exception:  # pylint: disable=broad-except
                plan = ""
        return {"logged_in": logged_in, "account": account, "plan": plan,
                "tier": self.api.user_tier if logged_in else 0, **self.state_data()}

    async def finish_login(self, result):
        if result.success:
            await self.enable_refresher()
            return {"step": "done"}
        if result.twofa_required:
            return {"step": "2fa"}
        return {"step": "failed", "error": "Incorrect username or password."}

    async def cmd_login(self, args):
        result = await self.api.login(args["username"], args["password"])
        return await self.finish_login(result)

    async def cmd_submit_2fa(self, args):
        result = await self.api.submit_2fa_code(args["code"])
        return await self.finish_login(result)

    async def cmd_logout(self, _args):
        if self.connector and self.connector.is_connection_active:
            await self.connector.disconnect()
            await self.wait_for_result(20)
        # Proton's logout also stops the refresher and clears its settings.
        await self.api.logout()
        self.refresher_enabled = False
        try:
            os.remove(os.path.join(CACHE, "countries.json"))
        except OSError:
            pass
        return {}

    def countries(self):
        tier = self.api.user_tier or 0
        found = {}
        for server in self.api.server_list.logicals:
            code = server.exit_country
            entry = found.setdefault(code, {"code": code, "name": server.exit_country_name,
                                            "cities": set(), "servers": 0, "available": False})
            entry["servers"] += 1
            if server.city:
                entry["cities"].add(server.city)
            if server.enabled and server.tier <= tier:
                entry["available"] = True
        result = []
        for entry in sorted(found.values(), key=lambda e: e["name"]):
            entry["cities"] = sorted(entry["cities"])
            result.append(entry)
        return result

    def write_countries(self):
        try:
            os.makedirs(CACHE, exist_ok=True)
            path = os.path.join(CACHE, "countries.json")
            with open(path + ".tmp", "w", encoding="utf-8") as handle:
                json.dump(self.countries(), handle)
            os.replace(path + ".tmp", path)
        except Exception as error:  # pylint: disable=broad-except
            print(f"spotty-vpn: could not cache countries: {error}", file=sys.stderr)

    async def cmd_countries(self, _args):
        self.require_login()
        await self.enable_refresher()
        return self.countries()

    def require_login(self):
        if not self.api.is_user_logged_in():
            raise RuntimeError("Sign in to Proton VPN first.")

    def pick_server(self, kind, value):
        servers = self.api.server_list
        if kind == "country":
            return servers.get_fastest_in_country(value.upper())
        if kind == "city":
            wanted = value.strip().lower()
            for server in servers.logicals:
                if server.city and server.city.lower() == wanted:
                    return servers.get_fastest_in_city(server.city)
            raise RuntimeError(f"No Proton VPN server in “{value}”.")
        if kind == "server":
            return servers.get_by_name(value.upper())
        return servers.get_fastest()

    async def cmd_connect(self, args):
        self.require_login()
        await self.enable_refresher()
        try:
            server = self.pick_server(args.get("kind", "fastest"), args.get("value", ""))
        except Exception as error:  # pylint: disable=broad-except
            message = str(error) or "No server is available on your plan for that choice."
            raise RuntimeError(message) from error
        vpn_server = self.connector.get_vpn_server(server, self.api.refresher.client_config)
        settings = await self.api.load_settings()
        await self.connector.connect(vpn_server, protocol=settings.protocol)
        return await self.wait_for_result(60)

    async def cmd_disconnect(self, _args):
        if not self.connector.is_connection_active:
            return self.state_data()
        await self.connector.disconnect()
        return await self.wait_for_result(30)

    def protocols(self):
        names = []
        # WireGuard and OpenVPN are Proton's "generic" group; "protun" needs
        # Proton's system plugin and is never available here.
        for group in ("generic",):
            for cls in self.connector.iter_available_protocols(group):
                protocol = getattr(cls, "protocol", None)
                if protocol and protocol not in names:
                    names.append(protocol)
        return [{"id": p, "label": PROTOCOL_LABELS.get(p, p)} for p in names]

    async def cmd_settings(self, _args):
        settings = await self.api.load_settings()
        features = settings.features
        return {
            "protocol": settings.protocol,
            "protocols": self.protocols(),
            "killswitch": int(settings.killswitch),
            "netshield": int(features.netshield),
            "vpn_accelerator": bool(features.vpn_accelerator),
            "moderate_nat": bool(features.moderate_nat),
            "port_forwarding": bool(features.port_forwarding),
            "ipv6": bool(settings.ipv6),
        }

    async def cmd_set_setting(self, args):
        key, value = args["key"], args["value"]
        settings = await self.api.load_settings()
        if key == "protocol":
            if value not in [p["id"] for p in self.protocols()]:
                raise RuntimeError("That protocol isn't available.")
            settings.protocol = value
        elif key == "killswitch":
            settings.killswitch = int(value)
        elif key == "ipv6":
            settings.ipv6 = bool(value)
        elif key == "netshield":
            settings.features.netshield = int(value)
        elif key in ("vpn_accelerator", "moderate_nat", "port_forwarding"):
            setattr(settings.features, key, bool(value))
        else:
            raise RuntimeError(f"Unknown setting {key}.")
        await self.api.save_settings(settings)
        return await self.cmd_settings({})


async def handle(client, message):
    request_id = message.get("id")
    handler = getattr(client, "cmd_" + str(message.get("cmd")), None)
    try:
        if handler is None:
            raise RuntimeError(f"Unknown command {message.get('cmd')}.")
        data = await handler(message.get("args") or {})
        emit({"id": request_id, "ok": True, "data": data})
    except ProtonAPIAuthenticationNeeded:
        await client.drop_expired_session()
        emit({"id": request_id, "ok": False,
              "error": "Your Proton session expired. Sign in again."})
    except Exception as error:  # pylint: disable=broad-except
        text = str(error) or type(error).__name__
        emit({"id": request_id, "ok": False, "error": text})


def read_requests(loop, client, stopped):
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except ValueError:
            continue
        asyncio.run_coroutine_threadsafe(handle(client, message), loop)
    loop.call_soon_threadsafe(stopped.set)


async def main():
    client = Client()
    await client.start()
    emit({"event": "ready", "data": await client.cmd_status({})})
    stopped = asyncio.Event()
    loop = asyncio.get_running_loop()
    threading.Thread(target=read_requests, args=(loop, client, stopped), daemon=True).start()
    # Spotty closing stdin ends the helper; the VPN connection itself is a
    # NetworkManager connection and stays up.
    await stopped.wait()
    await client.disable_refresher()


if __name__ == "__main__":
    asyncio.run(main())
