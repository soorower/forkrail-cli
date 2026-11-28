"""Copies a published forkrail release into this account, from inside it: the installer's
forkrail-install-fetch function (deploy/install/install-fetch.yaml), so the release (about 1 GB,
mostly the builder's image) goes from GitHub straight to the account and never through the
computer running the installer.

The release is one tar file (deploy/release/publish.sh). It's read as it downloads, never stored:
the builder image's layers go to the registry (ECR checks each against its digest), the other files
to the install's bucket under `prefix`, each with its SHA-256. The whole file's SHA-256 is checked
against the one the installer read from beside it; only then is the image tagged and the copy
reported done. The installer reads what happens from `status_key`, which this rewrites as it goes.

The event: {"url", "sha256", "bucket", "prefix", "status_key", "repository", "tag"}.
"""

import hashlib
import json
import posixpath
import queue
import re
import tarfile
import threading
import time
import urllib.request

import boto3
from botocore.config import Config

CONFIG = Config(retries={"max_attempts": 6, "mode": "standard"}, connect_timeout=10, read_timeout=120)
s3 = boto3.client("s3", config=CONFIG)
ecr = boto3.client("ecr", config=CONFIG)

# ECR takes a layer in parts of at least 5 MiB (the last may be smaller), in order.
PART = 10 << 20
# Blobs smaller than this (the image's config and manifests) are kept until the end.
SMALL = 1 << 20
# Layers sending at once, and each one's parts read ahead of the one sending: memory stays near
# LAYERS * (AHEAD + 1) * PART whatever the image's size.
LAYERS = 3
AHEAD = 2
# How often the status is rewritten while nothing else changes.
HEARTBEAT = 10
# The account this runs in, whose bucket alone it writes to (set by the handler).
OWNER = {}


def owned(**args):
    """S3 arguments that make a write fail unless the bucket is this account's own: its name is
    guessable, so another account could have made it first."""
    return {**args, "ExpectedBucketOwner": OWNER["account"]}


def checked(event, account):
    """The event, if it's one the installer sends (copier.rs `copy`): its own bucket and image
    repository, an https release, and files going only under the prefix that release's SHA-256
    names. Anyone else who may invoke this can't have it write a release of their choosing where
    an install will read one."""
    sha256 = event.get("sha256", "")
    if not re.fullmatch(r"[0-9a-f]{64}", sha256):
        raise ValueError("sha256 isn't a SHA-256")
    if not str(event.get("url", "")).startswith("https://"):
        raise ValueError("the release's url isn't https")
    if event.get("bucket") != f"forkrail-artifacts-{account}":
        raise ValueError("the bucket isn't this account's forkrail bucket")
    if event.get("repository") != "forkrail-builder":
        raise ValueError("the repository isn't forkrail-builder")
    if not re.fullmatch(r"install/release/[0-9A-Za-z.+-]+-" + sha256[:12] + "/", event.get("prefix", "")):
        raise ValueError("the prefix isn't the one this release's SHA-256 names")
    if not re.fullmatch(r"install/release/status-[0-9a-f]+\.json", event.get("status_key", "")):
        raise ValueError("the status key isn't a status file's")
    if not re.fullmatch(r"install-[0-9A-Za-z.+-]+", event.get("tag", "")):
        raise ValueError("the tag isn't an install's")
    return event


class Status:
    """The copy's progress, as the installer reads it: state, lines said so far, and a counter that
    moves at every write, so a copy that stopped (out of time, or memory) shows by standing still."""

    def __init__(self, bucket, key):
        self.bucket, self.key = bucket, key
        self.lock = threading.Lock()
        self.body = {"state": "running", "lines": [], "beat": 0}
        self.stopped = threading.Event()
        self.write()
        threading.Thread(target=self.beat, daemon=True).start()

    def write(self):
        with self.lock:
            self.body["beat"] += 1
            data = json.dumps(self.body).encode()
        s3.put_object(**owned(Bucket=self.bucket, Key=self.key, Body=data, ContentType="application/json"))

    def beat(self):
        while not self.stopped.wait(HEARTBEAT):
            try:
                self.write()
            except Exception:
                pass

    def say(self, line):
        print(line)
        with self.lock:
            self.body["lines"].append(line)
        self.write()

    def finish(self, **fields):
        self.stopped.set()
        with self.lock:
            self.body.update(fields)
        self.write()


class Hashing:
    """A stream that hashes what's read from it, and counts it."""

    def __init__(self, raw, on_read):
        self.raw, self.sha, self.got, self.on_read = raw, hashlib.sha256(), 0, on_read

    def read(self, n=-1):
        data = self.raw.read(n)
        self.sha.update(data)
        self.got += len(data)
        self.on_read(self.got)
        return data


def read_full(f, n):
    """Up to n bytes: fewer only at the end."""
    parts, left = [], n
    while left:
        data = f.read(left)
        if not data:
            break
        parts.append(data)
        left -= len(data)
    return b"".join(parts)


def available(repository, digest):
    out = ecr.batch_check_layer_availability(repositoryName=repository, layerDigests=[digest])
    return any(l.get("layerAvailability") == "AVAILABLE" for l in out.get("layers", []))


class Layer:
    """One layer going to ECR: its parts sent in order by a thread of its own, read ahead by the
    caller, which can go on to the next layer once the last part is queued."""

    def __init__(self, repository, digest):
        self.repository, self.digest = repository, digest
        self.upload = ecr.initiate_layer_upload(repositoryName=repository)["uploadId"]
        self.parts = queue.Queue(maxsize=AHEAD)
        self.error = None
        self.thread = threading.Thread(target=self.send, daemon=True)
        self.thread.start()

    def send(self):
        while True:
            item = self.parts.get()
            if item is None:
                break
            if self.error:
                continue
            first, chunk = item
            try:
                ecr.upload_layer_part(
                    repositoryName=self.repository,
                    uploadId=self.upload,
                    partFirstByte=first,
                    partLastByte=first + len(chunk) - 1,
                    layerPartBlob=chunk,
                )
            except Exception as e:
                self.error = e
        if self.error:
            return
        try:
            ecr.complete_layer_upload(repositoryName=self.repository, uploadId=self.upload, layerDigests=[self.digest])
        except ecr.exceptions.LayerAlreadyExistsException:
            pass
        except Exception as e:
            self.error = e

    def feed(self, f):
        first = 0
        while not self.error:
            chunk = read_full(f, PART)
            if not chunk:
                break
            self.parts.put((first, chunk))
            first += len(chunk)
        self.parts.put(None)

    def wait(self):
        self.thread.join()
        if self.error:
            raise RuntimeError(f"sending layer {self.digest}: {self.error}")


def relative(name):
    """A member's path inside the release's one top folder; "" for what isn't in it (macOS's ._
    files beside it, say), which is skipped; None for one that would land outside it."""
    parts = name.split("/", 1)
    if len(parts) != 2 or not parts[1]:
        return ""
    rel = posixpath.normpath(parts[1])
    if name.startswith("/") or rel == ".." or rel.startswith("../"):
        return None
    if posixpath.basename(rel).startswith("._"):
        return ""
    return rel


def image_manifest(index, blobs):
    """The image's manifest (the linux/arm64 one when the layout holds an index), as bytes, its
    media type and digest: as crates/cli/src/install/release.rs picks it."""
    descriptor = index["manifests"][0]
    kind = descriptor.get("mediaType", "")
    if "index" in kind or "manifest.list" in kind:
        inner = json.loads(blobs[descriptor["digest"]])
        descriptor = next(
            m for m in inner["manifests"] if m.get("platform", {}).get("architecture") == "arm64" and m.get("platform", {}).get("os") == "linux"
        )
    data = blobs[descriptor["digest"]]
    kind = descriptor.get("mediaType") or json.loads(data).get("mediaType") or "application/vnd.oci.image.manifest.v1+json"
    return data, kind, descriptor["digest"]


def copy(event, status):
    bucket, prefix, repository = event["bucket"], event["prefix"], event["repository"]
    request = urllib.request.Request(event["url"], headers={"User-Agent": "forkrail-install"})
    response = urllib.request.urlopen(request, timeout=60)
    whole = int(response.headers.get("Content-Length") or 0)
    shown = [0]

    def progress(got):
        step = max(whole // 10, 1) if whole else 100 << 20
        if got // step > shown[0]:
            shown[0] = got // step
            status.say(f"downloaded {got >> 20} of {whole >> 20} MB" if whole else f"downloaded {got >> 20} MB")

    stream = Hashing(response, progress)
    files, small, layout, layers = {}, {}, {}, []
    sent = skipped = 0
    with tarfile.open(fileobj=stream, mode="r|") as tar:
        for member in tar:
            if not member.isfile():
                continue
            rel = relative(member.name)
            if rel is None:
                raise RuntimeError(f"the release has a file outside its folder: {member.name}")
            if not rel:
                continue
            f = tar.extractfile(member)
            if rel.startswith("builder/blobs/"):
                algorithm, hex_ = rel.split("/")[-2:]
                digest = f"{algorithm}:{hex_}"
                if member.size < SMALL:
                    small[digest] = f.read()
                elif available(repository, digest):
                    skipped += 1
                else:
                    layer = Layer(repository, digest)
                    layer.feed(f)
                    layers.append(layer)
                    while len([l for l in layers if l.thread.is_alive()]) >= LAYERS:
                        next(l for l in layers if l.thread.is_alive()).thread.join(0.2)
                    for done in [l for l in layers if not l.thread.is_alive()]:
                        done.wait()
                        layers.remove(done)
                        sent += 1
            elif rel.startswith("builder/"):
                layout[rel] = f.read()
            else:
                hashed = Hashing(f, lambda _: None)
                s3.upload_fileobj(hashed, bucket, prefix + rel, ExtraArgs=owned())
                files[rel] = {"sha256": hashed.sha.hexdigest(), "size": hashed.got}
    for layer in layers:
        layer.wait()
        sent += 1
    # The rest of the file (the tar's end): all of it counts.
    while stream.read(1 << 20):
        pass
    got = stream.sha.hexdigest()
    if got != event["sha256"]:
        raise RuntimeError(f"the download isn't the release its SHA-256 names (got {got}, wanted {event['sha256']})")

    index = json.loads(layout["builder/index.json"])
    manifest_bytes, kind, digest = image_manifest(index, small)
    manifest = json.loads(manifest_bytes)
    for blob in [manifest["config"], *manifest["layers"]]:
        if blob["digest"] in small and not available(repository, blob["digest"]):
            layer = Layer(repository, blob["digest"])
            layer.parts.put((0, small[blob["digest"]]))
            layer.parts.put(None)
            layer.wait()
            sent += 1
        elif blob["digest"] not in small and not available(repository, blob["digest"]):
            raise RuntimeError(f"the release's image has no layer {blob['digest']}")
    status.say(f"builder image: {sent} layers sent, {skipped} already there")
    try:
        ecr.put_image(
            repositoryName=repository,
            imageManifest=manifest_bytes.decode(),
            imageManifestMediaType=kind,
            imageTag=event["tag"],
            imageDigest=digest,
        )
    except ecr.exceptions.ImageAlreadyExistsException:
        pass
    return {"files": files, "image_digest": digest, "sha256": got, "bytes": stream.got}


def handler(event, context):
    OWNER["account"] = context.invoked_function_arn.split(":")[4]
    event = checked(event, OWNER["account"])
    status = Status(event["bucket"], event["status_key"])
    started = time.time()
    try:
        result = copy(event, status)
        result["secs"] = round(time.time() - started)
        status.finish(state="done", **result)
    except Exception as e:
        print(f"failed: {type(e).__name__}: {e}")
        status.finish(state="failed", error=f"{type(e).__name__}: {e}")
