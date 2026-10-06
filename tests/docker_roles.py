#!/usr/bin/env python3
"""Smoke test the built server/client images on an isolated Docker network."""
import json
import subprocess
import time
import uuid
from multi_host import MCP, body, fails

def docker(*args):
    return subprocess.check_output(["docker", *args], text=True).strip()

def main():
    prefix = "localmcp-test-" + uuid.uuid4().hex[:10]
    network, config_volume = prefix+"-net", prefix+"-config"
    containers = []
    token = "integration-token-0123456789"
    try:
        docker("network", "create", network)
        docker("volume", "create", config_volume)
        for host in ["mac", "gpu"]:
            name = prefix+"-"+host
            docker("run", "-d", "--name", name, "--network", network, "--network-alias", host,
                "-e", "LOCAL_MCP_HOSTNAME="+host, "-e", "LOCAL_MCP_TOKEN="+token,
                "-e", "LOCAL_MCP_ALLOWED_HOSTS="+host, "localmcp-client:0.6.0")
            containers.append(name)
            assert docker("exec", name, "id", "-u") == "1000"
        registry = json.dumps({"connections":[
            {"hostname":host, "url":f"http://{host}:8080/local", "token_file":"/etc/local-mcp/token"}
            for host in ["mac","gpu"]]})
        docker("run", "--rm", "-u", "0", "--entrypoint", "sh",
            "-v", config_volume+":/etc/local-mcp", "localmcp-server:0.6.0",
            "-c", 'printf %s "$1" > /etc/local-mcp/connections.json; printf %s "$2" > /etc/local-mcp/token',
            "sh", registry, token)
        central = prefix+"-server"
        docker("run", "-d", "--name", central, "--network", network,
            "-p", "127.0.0.1::8080", "-v", config_volume+":/etc/local-mcp:ro",
            "-e", "LOCAL_MCP_CONNECTIONS_FILE=/etc/local-mcp/connections.json",
            "-e", "LOCAL_MCP_TOKEN="+token, "-e", "LOCAL_MCP_ALLOWED_HOSTS=localhost,127.0.0.1",
            "localmcp-server:0.6.0")
        containers.append(central)
        assert docker("exec", central, "id", "-u") == "1000"
        p = int(docker("port", central, "8080/tcp").rsplit(":",1)[1])
        for _ in range(60):
            try:
                client = MCP(p)
                info = json.loads(body(client.call("connections")))
                if all(c["status"]=="connected" for c in info["connections"]):
                    break
            except (OSError, AssertionError):
                pass
            time.sleep(.5)
        else:
            raise AssertionError("Docker roles did not become ready")
        body(client.call("write_file", connection="mac", path="same.txt", content="Mac container"))
        body(client.call("write_file", connection="gpu", path="same.txt", content="GPU container"))
        assert "Mac container" in body(client.call("read_file", connection="mac", path="same.txt"))
        assert "GPU container" in body(client.call("read_file", connection="gpu", path="same.txt"))
        fails(client.call("read_file", path="same.txt"))
        assert token not in json.dumps(info)
        print("PASS: built images run as UID 1000; central reaches two child containers and isolates file operations")
    finally:
        for name in reversed(containers):
            subprocess.run(["docker","rm","-f",name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["docker","volume","rm",config_volume], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        subprocess.run(["docker","network","rm",network], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

if __name__ == "__main__":
    main()
