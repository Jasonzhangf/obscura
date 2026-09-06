"""Actual Host -> raw socket -> H.264 -> decode acceptance, no remote claim.

Usage: python3 host-replay.py HOST_BINARY MEDIA_BINARY NEW_EVIDENCE_DIRECTORY
Requires FFmpeg with libx264. Only starts/stops its own isolated test processes.
"""
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time


def receive(reader):
    header = reader.readline(4096)
    assert header.endswith(b"\n") and len(header) < 4096, header
    return json.loads(header)


def main():
    host_bin, media_bin, evidence = sys.argv[1:]
    evidence = pathlib.Path(evidence)
    evidence.mkdir()
    with tempfile.TemporaryDirectory(prefix="om-", dir="/tmp") as directory:
        root = pathlib.Path(directory)
        video = socket.socket(socket.AF_UNIX)
        video.bind(str(root / "video"))
        video.listen(1)
        video.settimeout(10)
        host = subprocess.Popen([host_bin, "--socket-dir", str(root / "host")], stdout=subprocess.DEVNULL,
                                stderr=(evidence / "host.log").open("wb"))
        media = None
        control = socket.socket(socket.AF_UNIX)
        control.settimeout(10)
        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    control.connect(str(root / "host" / "host.sock"))
                    break
                except (FileNotFoundError, ConnectionRefusedError):
                    assert time.monotonic() < deadline and host.poll() is None
                    time.sleep(0.02)
            replies = control.makefile("rb")
            assert receive(replies)["version"] == 4

            def request(command, operation=None):
                control.sendall(json.dumps({"id": 1, "command": command, "operation": operation}).encode() + b"\n")
                reply = receive(replies)
                assert reply["type"] == "result", reply
                return reply["value"]

            def mutate(command):
                state = request({"type": "status"})
                operation = {k: state[k] for k in ["session_id", "attachment_id", "viewport_revision", "document_revision"]}
                operation.update(sequence=state["next_sequence"], control_epoch=state["control"]["epoch"])
                return request(command, operation)

            request({"type": "attach", "mode": "agent"})
            mutate({"type": "resize", "width": 160, "height": 120})
            state = mutate({"type": "navigate", "url": "data:text/html,<style>body{margin:0;background:white}button{width:40px;height:40px;border:0;background:red}</style><button onclick=\"this.style.background='lime'\"></button><input id='field' value='retained'>"})
            media = subprocess.Popen([media_bin, "--frames-socket", str(root / "host" / "frames.sock"),
                                      "--video-socket", str(root / "video")], stdout=subprocess.DEVNULL,
                                     stderr=(evidence / "media.log").open("wb"))
            connection, _ = video.accept()
            connection.settimeout(10)
            frames = connection.makefile("rb")

            def frame():
                while True:
                    packet = receive(frames)
                    assert packet["type"] in ("waiting", "access_unit"), packet
                    if packet["type"] == "access_unit":
                        assert packet["byte_length"] <= 4 * 1024 * 1024
                        data = frames.read(packet["byte_length"])
                        assert len(data) == packet["byte_length"]
                        return packet, data

            def decoded(packet, data):
                result = subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "h264", "-i", "pipe:0",
                                         "-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"],
                                        input=data, capture_output=True, timeout=5, check=True)
                assert len(result.stdout) == packet["coded_width"] * packet["coded_height"] * 3
                offset = (20 * packet["coded_width"] + 20) * 3
                return result.stdout[offset:offset + 3]

            packet, data = frame()
            assert packet["source"]["session_id"] == state["session_id"]
            assert decoded(packet, data)[0] > 220
            (evidence / "before.h264").write_bytes(data)
            mutate({"type": "click", "x": 20, "y": 20})
            deadline = time.monotonic() + 5
            while True:
                packet, data = frame()
                pixel = decoded(packet, data)
                if pixel[1] > 220 and pixel[0] < 30:
                    break
                assert time.monotonic() < deadline, pixel
            (evidence / "after.h264").write_bytes(data)
            resized = mutate({"type": "resize", "width": 391, "height": 845})
            deadline = time.monotonic() + 5
            while True:
                packet, data = frame()
                if packet["source"]["viewport_revision"] == resized["viewport_revision"]:
                    break
                assert time.monotonic() < deadline
            assert (packet["coded_width"], packet["coded_height"]) == (392, 846)
            assert packet["source"]["document_revision"] == state["document_revision"]
            assert mutate({"type": "evaluate", "expression": "document.getElementById('field').value"})["result"]["value"] == "retained"
            decoded(packet, data)
            (evidence / "resized.h264").write_bytes(data)
            (evidence / "resized.json").write_text(json.dumps(packet, indent=2))
            mutate({"type": "close_session"})
            while True:
                packet = receive(frames)
                if packet["type"] == "closed":
                    break
                if packet["type"] == "access_unit":
                    frames.read(packet["byte_length"])
            assert media.wait(timeout=5) == 0
            frames.close()
            connection.close()
            replies.close()
            print(json.dumps({"pass": True, "flow": "Host -> raw RGBA -> H264 -> decoded pixel change -> resize preserves form -> close",
                              "evidence": str(evidence)}))
        finally:
            control.close()
            video.close()
            for child in [media, host]:
                if child is not None and child.poll() is None:
                    child.terminate()
                    child.wait(timeout=10)


if __name__ == "__main__":
    main()
