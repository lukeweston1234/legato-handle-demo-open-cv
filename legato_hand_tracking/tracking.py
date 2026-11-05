"""Webcam hand tracking -> UDP packets for the Rust audio process.

Every camera frame sends one datagram (little-endian), even with no hands, so
the audio process always knows where to send its status back:

    u64  timestamp_us
    u8   hand_count
    per hand:
        u8   handedness   (0 = left, 1 = right)
        f32  score
        f32  x, y, z      x 21 landmarks (normalized image coords)

i.e. 9 + hand_count * 257 bytes. timestamp_us is when the frame was captured
(monotonic, relative to tracker start). The image is mirrored, so x grows to
the user's right and handedness matches the user's actual hands.

The audio process replies to the same socket with a status packet (see
`src/status.rs`), which the preview draws as an overlay.
"""

import argparse
from dataclasses import dataclass
import socket
import struct
import threading
import time
import urllib.request
from pathlib import Path

import cv2
import mediapipe as mp
from mediapipe.tasks.python import BaseOptions, vision

MODEL_URL = "https://storage.googleapis.com/mediapipe-models/hand_landmarker/hand_landmarker/float16/latest/hand_landmarker.task"
MODEL_PATH = Path(__file__).parent / "models" / "hand_landmarker.task"

HEADER = struct.Struct("<QB")
HAND = struct.Struct("<Bf" + "fff" * 21)
STATUS_HEAD = struct.Struct("<BBBfffff")
STATUS_VERSION = 2

FINGER_NAMES = ["index", "middle", "ring", "pinky"]


def ensure_model() -> Path:
    if not MODEL_PATH.exists():
        MODEL_PATH.parent.mkdir(parents=True, exist_ok=True)
        print(f"downloading {MODEL_URL}")
        urllib.request.urlretrieve(MODEL_URL, MODEL_PATH)
    return MODEL_PATH


def encode(result: vision.HandLandmarkerResult, timestamp_us: int) -> bytes:
    out = bytearray(HEADER.pack(timestamp_us, len(result.hand_landmarks)))
    for landmarks, handedness in zip(result.hand_landmarks, result.handedness):
        category = handedness[0]
        coords = [c for lm in landmarks for c in (lm.x, lm.y, lm.z)]
        out += HAND.pack(category.category_name == "Right", category.score, *coords)
    return bytes(out)


@dataclass
class Status:
    curled: list[bool]
    sounding: list[bool]
    hold: bool
    fist: bool
    height: float
    pinch: float
    roll: float
    pos: float
    depth: float
    names: list[str]


def decode_status(buf: bytes) -> Status | None:
    try:
        version, curled, flags, height, pinch, roll, pos, depth = STATUS_HEAD.unpack_from(buf)
        if version != STATUS_VERSION:
            return None
        at = STATUS_HEAD.size
        names = []
        for _ in range(4):
            n = buf[at]
            names.append(buf[at + 1 : at + 1 + n].decode("utf-8", "replace"))
            at += 1 + n
    except (struct.error, IndexError):
        return None
    bits = lambda b: [bool(b >> i & 1) for i in range(4)]
    return Status(
        bits(curled), bits(flags), bool(flags & 16), bool(flags & 32),
        height, pinch, roll, pos, depth, names,
    )


class Overlay:
    """Draws the latest status from the audio process over the preview."""

    def __init__(self) -> None:
        self.status: Status | None = None
        self.received_at = 0.0

    def update(self, status: Status, now: float) -> None:
        self.status, self.received_at = status, now

    def draw(self, img, now: float) -> None:
        h, w = img.shape[:2]
        st = self.status
        if st is None or now - self.received_at > 1.0:
            text(img, "waiting for the audio process...", (12, h - 16), 0.6, (180, 180, 180))
            return

        shade(img, 0, h - 150, w, h)

        # The chord: what's sounding, and which fingers give it.
        chord = "+".join(n for n, on in zip(st.names, st.sounding) if on) or "A minor pentatonic"
        text(img, chord, (12, h - 108), 1.1, (255, 255, 255), 2)
        badges = [label for label, on in (("HOLD", st.hold), ("HUSH", st.fist)) if on]
        if badges:
            text(img, "  ".join(badges), (12, h - 80), 0.6, (80, 200, 255), 2)
        for i, name in enumerate(st.names):
            x, y = 12 + i * 70, h - 62
            colour = (80, 220, 120) if st.sounding[i] else (120, 120, 120)
            cv2.rectangle(img, (x, y), (x + 62, y + 40), colour, cv2.FILLED if st.sounding[i] else 2)
            ink = (20, 20, 20) if st.sounding[i] else (220, 220, 220)
            text(img, name, (x + 8, y + 24), 0.6, ink, 2)
            text(img, FINGER_NAMES[i], (x + 8, y + 36), 0.35, ink)

        # The conducting hand, as meters down the right edge.
        meters = [
            ("busy", st.height), ("tone", st.pinch), ("attack", st.roll),
            ("decay", st.pos), ("verb", st.depth),
        ]
        for i, (label, value) in enumerate(meters):
            x, top, bottom = w - 186 + i * 36, 20, 200
            shade(img, x - 4, top - 4, x + 24, bottom + 20)
            fill_top = int(bottom - value * (bottom - top))
            cv2.rectangle(img, (x, fill_top), (x + 20, bottom), (255, 180, 60), cv2.FILLED)
            text(img, label, (x - 2, bottom + 14), 0.35, (230, 230, 230))


def text(img, s: str, org, scale: float, colour, thickness: int = 1) -> None:
    cv2.putText(img, s, org, cv2.FONT_HERSHEY_SIMPLEX, scale, colour, thickness, cv2.LINE_AA)


def shade(img, x0: int, y0: int, x1: int, y1: int) -> None:
    """Darkens a rectangle so the overlay reads over any background."""
    region = img[max(y0, 0) : y1, max(x0, 0) : x1]
    region[:] = (region * 0.45).astype(region.dtype)


def draw(img, result: vision.HandLandmarkerResult) -> None:
    h, w = img.shape[:2]
    for landmarks in result.hand_landmarks:
        points = [(int(lm.x * w), int(lm.y * h)) for lm in landmarks]
        for conn in vision.HandLandmarksConnections.HAND_CONNECTIONS:
            cv2.line(img, points[conn.start], points[conn.end], (0, 255, 0), 2)
        for p in points:
            cv2.circle(img, p, 4, (0, 0, 255), cv2.FILLED)
        cv2.circle(img, points[0], 10, (255, 0, 255), cv2.FILLED)


class LatestFrame:
    """Reads the camera on a background thread and keeps only the newest frame.

    cv2.VideoCapture otherwise queues frames in the driver, so whenever
    inference is slower than the camera, every read hands back a stale frame.
    """

    def __init__(self, index: int, width: int, height: int, fps: int) -> None:
        self.cap = cv2.VideoCapture(index, cv2.CAP_V4L2)
        self.cap.set(cv2.CAP_PROP_FOURCC, cv2.VideoWriter_fourcc(*"MJPG"))
        self.cap.set(cv2.CAP_PROP_FRAME_WIDTH, width)
        self.cap.set(cv2.CAP_PROP_FRAME_HEIGHT, height)
        self.cap.set(cv2.CAP_PROP_FPS, fps)
        self.cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)
        if not self.cap.isOpened():
            raise RuntimeError(f"could not open camera {index}")

        self._cond = threading.Condition()
        self._frame = None
        self._captured_at = 0.0
        self._seq = 0
        self._running = True
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        while self._running:
            success, img = self.cap.read()
            captured_at = time.monotonic()
            with self._cond:
                if not success:
                    self._running = False
                else:
                    self._frame, self._captured_at = img, captured_at
                    self._seq += 1
                self._cond.notify_all()

    def read(self, after_seq: int):
        """Block until a frame newer than after_seq exists; None if the camera stopped."""
        with self._cond:
            self._cond.wait_for(lambda: self._seq > after_seq or not self._running)
            if self._seq <= after_seq:
                return None
            return self._seq, self._frame, self._captured_at

    def release(self) -> None:
        self._running = False
        self._thread.join(timeout=1)
        self.cap.release()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--camera", type=int, default=0)
    parser.add_argument("--addr", default="127.0.0.1:9870", help="UDP target host:port")
    parser.add_argument("--hands", type=int, default=2)
    parser.add_argument("--width", type=int, default=640)
    parser.add_argument("--height", type=int, default=480)
    parser.add_argument("--fps", type=int, default=60, help="requested; the camera may deliver less")
    parser.add_argument("--no-preview", action="store_true")
    args = parser.parse_args()

    host, port = args.addr.rsplit(":", 1)
    target = (host, int(port))
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setblocking(False)
    overlay = Overlay()

    options = vision.HandLandmarkerOptions(
        base_options=BaseOptions(model_asset_path=str(ensure_model())),
        running_mode=vision.RunningMode.VIDEO,
        num_hands=args.hands,
    )

    start = time.monotonic()
    camera = LatestFrame(args.camera, args.width, args.height, args.fps)
    prev = start
    seq = 0

    with vision.HandLandmarker.create_from_options(options) as landmarker:
        while (latest := camera.read(seq)) is not None:
            seq, img, captured_at = latest

            img = cv2.flip(img, 1)
            timestamp_us = int((captured_at - start) * 1_000_000)
            rgb = cv2.cvtColor(img, cv2.COLOR_BGR2RGB)
            frame = mp.Image(image_format=mp.ImageFormat.SRGB, data=rgb)
            result = landmarker.detect_for_video(frame, timestamp_us // 1000)

            try:
                sock.sendto(encode(result, timestamp_us), target)
            except OSError:
                pass  # the audio process isn't up yet

            # Keep only the newest status; older ones are already stale.
            while True:
                try:
                    status = decode_status(sock.recv(4096))
                except (BlockingIOError, ConnectionRefusedError):
                    break
                if status is not None:
                    overlay.update(status, time.monotonic())

            if args.no_preview:
                continue

            now = time.monotonic()
            fps = 1 / max(now - prev, 1e-6)
            prev = now
            age_ms = (now - captured_at) * 1000
            draw(img, result)
            overlay.draw(img, now)
            cv2.putText(img, f"{fps:.0f} fps  {age_ms:.0f} ms", (10, 40), cv2.FONT_HERSHEY_PLAIN, 2, (255, 0, 255), 2)
            cv2.imshow("Image", img)
            if cv2.waitKey(1) & 0xFF == ord("q"):
                break
            # imshow recreates a window closed with the title-bar X, so check for it.
            if cv2.getWindowProperty("Image", cv2.WND_PROP_VISIBLE) < 1:
                break

    camera.release()
    cv2.destroyAllWindows()


if __name__ == "__main__":
    main()
