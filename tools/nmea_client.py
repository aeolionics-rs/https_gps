#!/usr/bin/env python3
"""Test client for the ESP32 NMEA stream.

Opens several concurrent HTTP connections to the board, decodes the chunked
NMEA stream on each, checks every sentence's checksum, and prints per-connection
statistics periodically. Uses only the Python standard library.

Examples:
    tools/nmea_client.py 192.168.1.50                # 4 connections, stats every 5 s
    tools/nmea_client.py 192.168.1.50 -n 5           # one more than the board's 4 slots
    tools/nmea_client.py 192.168.1.50 -n 1 --show    # print the sentences too
    tools/nmea_client.py 192.168.1.50 --reconnect --duration 60
"""

import argparse
import asyncio
import functools
import os
import sys
import time

print = functools.partial(print, flush=True)


class Conn:
    """State and counters for one connection."""

    def __init__(self, cid):
        self.cid = cid
        self.state = "connecting"
        self.connects = 0
        self.sentences = 0
        self.bad_checksums = 0
        self.bytes = 0
        self.last_sentence_at = None
        self.window_sentences = 0  # since the last stats report

    def label(self):
        return f"[{self.cid}]"


def checksum_ok(sentence):
    """Return True if `sentence` ($...*hh) has a valid NMEA checksum."""
    if not sentence.startswith("$") or len(sentence) < 4 or sentence[-3] != "*":
        return False
    calculated = 0
    for ch in sentence[1:-3]:
        calculated ^= ord(ch)
    try:
        return calculated == int(sentence[-2:], 16)
    except ValueError:
        return False


async def read_head(reader):
    """Read the status line and headers; return (status, headers)."""
    status_line = (await reader.readline()).decode("latin-1").strip()
    if not status_line:
        raise ConnectionError("connection closed before response")
    headers = {}
    while True:
        line = (await reader.readline()).decode("latin-1").strip()
        if not line:
            break
        name, _, value = line.partition(":")
        headers[name.strip().lower()] = value.strip()
    parts = status_line.split(" ", 2)
    if len(parts) < 2 or not parts[1].isdigit():
        raise ConnectionError(f"bad status line: {status_line!r}")
    return int(parts[1]), headers


async def body_chunks(reader, headers):
    """Yield the response body as it arrives, decoding chunked encoding."""
    if headers.get("transfer-encoding", "").lower() == "chunked":
        while True:
            size_line = (await reader.readline()).decode("latin-1").strip()
            if not size_line:
                return
            size = int(size_line.split(";", 1)[0], 16)
            if size == 0:
                return
            data = await reader.readexactly(size)
            await reader.readexactly(2)  # CR LF after each chunk
            yield data
    else:
        while data := await reader.read(4096):
            yield data


async def stream(conn, args):
    """Connect once and consume the stream until it ends."""
    conn.state = "connecting"
    reader, writer = await asyncio.wait_for(
        asyncio.open_connection(args.host, args.port), args.timeout
    )
    try:
        conn.connects += 1
        request = (
            f"GET {args.path} HTTP/1.1\r\n"
            f"Host: {args.host}\r\n"
            "Accept: text/plain\r\n"
            "\r\n"
        )
        writer.write(request.encode())
        await writer.drain()

        status, headers = await asyncio.wait_for(read_head(reader), args.timeout)
        if status != 200:
            raise ConnectionError(f"HTTP {status}")
        conn.state = "streaming"
        print(f"{conn.label()} connected ({headers.get('content-type', '?')})")

        pending = b""
        chunks = body_chunks(reader, headers)
        while True:
            # A silent connection usually means the board has stopped sending.
            try:
                data = await asyncio.wait_for(anext(chunks), args.timeout)
            except StopAsyncIteration:
                break
            except BaseException:
                await chunks.aclose()
                raise
            conn.bytes += len(data)
            pending += data
            *lines, pending = pending.split(b"\n")
            for raw in lines:
                sentence = raw.rstrip(b"\r").decode("ascii", errors="replace")
                if not sentence:
                    continue
                conn.sentences += 1
                conn.window_sentences += 1
                conn.last_sentence_at = time.monotonic()
                ok = checksum_ok(sentence)
                if not ok:
                    conn.bad_checksums += 1
                if args.show or not ok:
                    flag = "" if ok else "  <-- bad checksum"
                    print(f"{conn.label()} {sentence}{flag}")
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except OSError:
            pass


async def run_conn(conn, args):
    """Run one connection, reconnecting if requested."""
    while True:
        try:
            await stream(conn, args)
            conn.state = "closed"
            print(f"{conn.label()} server closed the stream")
        except asyncio.TimeoutError:
            conn.state = "timeout"
            print(f"{conn.label()} timed out after {args.timeout:g} s")
        except (OSError, ConnectionError, asyncio.IncompleteReadError, ValueError) as e:
            conn.state = "error"
            print(f"{conn.label()} {type(e).__name__}: {e}")
        if not args.reconnect:
            return
        await asyncio.sleep(args.reconnect_delay)


async def report(conns, interval):
    """Print a statistics table every `interval` seconds."""
    last = time.monotonic()
    while True:
        await asyncio.sleep(interval)
        now = time.monotonic()
        elapsed = now - last
        last = now
        print(
            f"--- {time.strftime('%H:%M:%S')} "
            f"{'conn':>4} {'state':>10} {'conns':>5} {'sentences':>9} "
            f"{'rate/s':>6} {'bad':>4} {'bytes':>9} {'silent':>6}"
        )
        for c in conns:
            silent = (
                f"{now - c.last_sentence_at:5.1f}s" if c.last_sentence_at else "     -"
            )
            print(
                f"    {'':8} {c.cid:>4} {c.state:>10} {c.connects:>5} {c.sentences:>9} "
                f"{c.window_sentences / elapsed:>6.1f} {c.bad_checksums:>4} "
                f"{c.bytes:>9} {silent:>6}"
            )
            c.window_sentences = 0


async def main(args):
    conns = [Conn(i) for i in range(args.connections)]
    tasks = []
    for conn in conns:
        tasks.append(asyncio.create_task(run_conn(conn, args)))
        if args.stagger:
            await asyncio.sleep(args.stagger)
    reporter = asyncio.create_task(report(conns, args.interval))

    interrupted = False
    try:
        if args.duration:
            await asyncio.wait(tasks, timeout=args.duration)
        else:
            await asyncio.gather(*tasks)
    except asyncio.CancelledError:
        # Ctrl-C: asyncio.run cancels this task. Fall through to the summary.
        interrupted = True
    finally:
        reporter.cancel()
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, reporter, return_exceptions=True)

    total = sum(c.sentences for c in conns)
    bad = sum(c.bad_checksums for c in conns)
    print(f"done: {total} sentences over {len(conns)} connection(s), {bad} bad checksums")
    if interrupted:
        return 130
    return 1 if bad or any(c.connects == 0 for c in conns) else 0


def parse_args():
    p = argparse.ArgumentParser(
        description="Open several connections to the ESP32 NMEA stream and check them."
    )
    p.add_argument("host", help="board IP address or hostname")
    p.add_argument("-p", "--port", type=int, default=80, help="HTTP port (default 80)")
    p.add_argument("--path", default="/", help="request path (default /)")
    p.add_argument(
        "-n", "--connections", type=int, default=4,
        help="number of concurrent connections (default 4, the board's limit)",
    )
    p.add_argument(
        "--stagger", type=float, default=0.2, metavar="SECS",
        help="delay between opening connections (default 0.2)",
    )
    p.add_argument(
        "-d", "--duration", type=float, metavar="SECS",
        help="stop after this many seconds (default: run until Ctrl-C)",
    )
    p.add_argument(
        "-i", "--interval", type=float, default=5, metavar="SECS",
        help="statistics interval (default 5)",
    )
    p.add_argument(
        "-t", "--timeout", type=float, default=10, metavar="SECS",
        help="connect timeout, and how long a stream may stay silent (default 10)",
    )
    p.add_argument("-s", "--show", action="store_true", help="print every sentence")
    p.add_argument(
        "-r", "--reconnect", action="store_true",
        help="reconnect when a connection ends or fails",
    )
    p.add_argument(
        "--reconnect-delay", type=float, default=2, metavar="SECS",
        help="delay before reconnecting (default 2)",
    )
    return p.parse_args()


if __name__ == "__main__":
    try:
        sys.exit(asyncio.run(main(parse_args())))
    except BrokenPipeError:
        # Output was piped into something like `head` that has exited.
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        sys.exit(1)
