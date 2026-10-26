"""Run a Browser Use agent against Chromium isolated inside Shuru.

The browser recipe runs headless Chromium as a guest service, so the VM is
ready when the command starts and the only thing to do is keep it alive.
"""

import asyncio
import os
import socket
import subprocess
import time
import urllib.request

from browser_use import Agent
from browser_use.browser import BrowserProfile, BrowserSession
from browser_use.llm import ChatOpenAI


def free_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_for_cdp(url: str, process: subprocess.Popen[bytes]) -> None:
    # The first run builds the browser checkpoint from the recipe, which
    # takes a few minutes; later runs answer within seconds.
    deadline = time.monotonic() + 600
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"Shuru exited with code {process.returncode}")
        try:
            with urllib.request.urlopen(f"{url}/json/version", timeout=1) as response:
                if response.status == 200:
                    return
        except OSError:
            time.sleep(0.25)
    raise RuntimeError(f"Chromium CDP did not become ready at {url}")


async def run_agent(cdp_url: str) -> None:
    browser = BrowserSession(
        browser_profile=BrowserProfile(cdp_url=cdp_url, is_local=False)
    )
    agent = Agent(
        task="Open https://example.com and return its heading.",
        llm=ChatOpenAI(model=os.getenv("OPENAI_MODEL", "gpt-5.6-luna"), reasoning_effort="xhigh"),
        browser_session=browser,
    )
    history = await agent.run(max_steps=4)
    result = history.final_result() or ""
    print(f"Browser Use result: {result}")
    if "Example Domain" not in result:
        raise RuntimeError("Browser Use did not return the expected heading")


def main() -> None:
    if not os.getenv("OPENAI_API_KEY"):
        raise RuntimeError("OPENAI_API_KEY is required")

    host_port = free_port()
    cdp_url = f"http://127.0.0.1:{host_port}"
    process = subprocess.Popen(
        [
            "shuru",
            "run",
            "--recipe",
            os.getenv("SHURU_BROWSER_RECIPE", "browser"),
            "--allow-net",
            "--port",
            f"{host_port}:9222",
            "--memory",
            "4096",
            "--",
            "sleep",
            "infinity",
        ]
    )

    try:
        wait_for_cdp(cdp_url, process)
        print(f"Shuru Chromium CDP: {cdp_url}")
        asyncio.run(run_agent(cdp_url))
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


if __name__ == "__main__":
    main()
