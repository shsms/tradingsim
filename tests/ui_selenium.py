#!/usr/bin/python3
"""Selenium smoke test for the tradingsim UI.

Boots the tradingsim binary on random ports under a minimal lisp
config, opens the page in headless Firefox, and asserts the post-
redesign layout + gridpool master-detail behaviour: pool list auto-
selects a row, the "Public order book" label is in place, and the
order / trade panes render the right empty-state copy.

Run with `/usr/bin/python3` directly so the system python3-selenium
in /usr/lib/python3/dist-packages is on sys.path — the project's
venv at /vagrant/venv masks dist-packages from its own python.
See CLAUDE.md ("Browser tests").
"""

import os
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.firefox.options import Options
from selenium.webdriver.firefox.service import Service
from selenium.webdriver.support import expected_conditions as EC
from selenium.webdriver.support.ui import WebDriverWait

REPO = Path(__file__).resolve().parents[1]
BIN = REPO / "target" / "debug" / "tradingsim"


def free_port() -> int:
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def write_config(path: Path, ui_port: int, trading_port: int, weather_port: int) -> None:
    path.write_text(
        f'(set-trading-addr "[::1]:{trading_port}")\n'
        f'(set-ui-addr "127.0.0.1:{ui_port}")\n'
        f'(set-weather-addr "[::1]:{weather_port}")\n'
    )


def wait_for_http(url: str, timeout: float = 20.0) -> None:
    end = time.time() + timeout
    last: Exception | None = None
    while time.time() < end:
        try:
            with urllib.request.urlopen(url, timeout=0.5) as r:
                if r.status < 500:
                    return
        except Exception as e:
            last = e
            time.sleep(0.2)
    raise RuntimeError(f"timed out waiting for {url}: {last}")


def make_driver() -> webdriver.Firefox:
    opts = Options()
    opts.add_argument("-headless")
    # Approximate a 32" 4K target audience — wide enough that the
    # full layout (chart, gridpool drill-down, filters, weather +
    # book row, trades tape) lays out as designed instead of falling
    # back to the narrow-viewport stacks.
    opts.add_argument("--width=2560")
    opts.add_argument("--height=1440")
    opts.binary_location = "/usr/bin/firefox-esr"
    # Bypass Selenium Manager — the python3-selenium package doesn't
    # ship a manager binary, and geckodriver lives at a known path
    # (installed manually from Mozilla's GitHub releases per
    # CLAUDE.md).
    service = Service(executable_path="/usr/local/bin/geckodriver")
    return webdriver.Firefox(service=service, options=opts)


def main() -> int:
    assert BIN.exists(), f"missing {BIN} — run `cargo build` first"
    cfg = REPO / "target" / "ui-selenium-config.lisp"
    ui_port, trading_port, weather_port = free_port(), free_port(), free_port()
    write_config(cfg, ui_port, trading_port, weather_port)

    env = {**os.environ, "RUST_LOG": "warn"}
    proc = subprocess.Popen(
        [str(BIN), str(cfg)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        env=env,
        cwd=REPO,
    )
    try:
        wait_for_http(f"http://127.0.0.1:{ui_port}/api/info")
        driver = make_driver()
        try:
            driver.set_page_load_timeout(20)
            driver.get(f"http://127.0.0.1:{ui_port}/")
            wait = WebDriverWait(driver, 10)

            assert "tradingsim" in driver.title

            # Auto-select fires on the first /api/gridpools poll —
            # wait for the selected row to appear.
            wait.until(
                EC.presence_of_element_located(
                    (By.CSS_SELECTOR, "#gridpool-list .row-item.selected")
                )
            )
            list_rows = driver.find_elements(By.CSS_SELECTOR, "#gridpool-list .row-item")
            assert list_rows, "expected at least one pool row"

            book_head = driver.find_element(By.CSS_SELECTOR, ".panel-book h2")
            # h2 carries `text-transform: uppercase`; compare lower.
            assert "public order book" in book_head.text.lower(), book_head.text

            assert driver.find_element(By.ID, "gridpool-period-select")

            # The default pool has no user-submitted orders so the
            # middle pane shows the empty-state copy. The right pane
            # follows ("select an order" since selecting a pool clears
            # any prior order selection).
            orders_pane = driver.find_element(By.ID, "gridpool-orders")
            assert "no orders" in orders_pane.text.lower(), orders_pane.text
            trades_pane = driver.find_element(By.ID, "gridpool-trades")
            assert "select an order" in trades_pane.text.lower(), trades_pane.text

            # The gridpool panel sits above the area filter — locks in
            # the "drill-down is portfolio-scoped, filters scope the
            # public-market panels below" reading order.
            gp_y = driver.find_element(By.CSS_SELECTOR, ".panel-gridpools").rect["y"]
            filter_y = driver.find_element(By.ID, "area-filter-bar").rect["y"]
            assert gp_y < filter_y, (gp_y, filter_y)

            # Weather + book share a row. Compare top edges with a few
            # pixels of slack to absorb sub-pixel rounding.
            weather_y = driver.find_element(By.CSS_SELECTOR, ".panel-weather").rect["y"]
            book_y = driver.find_element(By.CSS_SELECTOR, ".panel-book").rect["y"]
            assert abs(weather_y - book_y) < 4, (weather_y, book_y)

            shot = REPO / "target" / "ui-selenium-screenshot.png"
            driver.save_screenshot(str(shot))
            print(f"OK (screenshot: {shot})")
            return 0
        finally:
            driver.quit()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        cfg.unlink(missing_ok=True)


if __name__ == "__main__":
    sys.exit(main())
