"""Webcam hand tracking -> UDP packets for the Rust audio process.

Each frame with at least one hand sends one datagram (little-endian):

    u64  timestamp_us
    u8   hand_count
    per hand:
        u8   handedness   (0 = left, 1 = right)
        f32  score
        f32  x, y, z      x 21 landmarks (normalized image coords)

i.e. 9 + hand_count * 257 bytes. timestamp_us is when the frame was captured
(monotonic, relative to tracker start). The image is mirrored, so x grows to
the user's right and handedness matches the user's actual hands.
"""

import argparse
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

            if result.hand_landmarks:
                sock.sendto(encode(result, timestamp_us), target)

            if args.no_preview:
                continue

            now = time.monotonic()
            fps = 1 / max(now - prev, 1e-6)
            prev = now
            age_ms = (now - captured_at) * 1000
            draw(img, result)
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
