#!/usr/bin/env python3
# Copyright (c) 2026 ZyvorAI Labs Private Limited.
# SPDX-License-Identifier: LicenseRef-Zyvor-Production-1.0
"""Upload the Atlas Storage Center demo MP4 to YouTube via Data API v3.

Companion to scripts/demo/record-atlas-demo.mjs. Reuses the same OAuth token/client-secrets
convention as the Zeus OS uploader (see ~/Desktop/Zeus-OS-Demo-Videos/.youtube-upload/).

Usage:
    python3 scripts/demo/upload-atlas-demo.py <video.mp4> --token /path/to/token.json [--privacy public]
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

from google.auth.transport.requests import Request
from google.oauth2.credentials import Credentials
from googleapiclient.discovery import build
from googleapiclient.http import MediaFileUpload

SCOPES = ["https://www.googleapis.com/auth/youtube"]

TITLE = "Atlas · Storage Center — Zyvor Storage Control Plane (Client Demo)"
DESCRIPTION = (
    "Atlas — the storage control plane for the Zyvor product suite.\n"
    "Command Deck, Volumes, DataBridge (6 source engines), Observatory, and native Ceph "
    "introspection — one console for the fleet.\n\n"
    "Recorded from a live Atlas deployment.\n"
    "#Atlas #Ceph #Storage #Kubernetes #DataBridge"
)
TAGS = ["Atlas", "Ceph", "Storage", "Kubernetes", "DataBridge", "demo"]


def get_creds(token_path: Path) -> Credentials:
    creds = Credentials.from_authorized_user_file(str(token_path), SCOPES)
    if creds.expired and creds.refresh_token:
        creds.refresh(Request())
        token_path.write_text(creds.to_json())
    return creds


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("video", type=Path)
    ap.add_argument("--token", required=True, type=Path)
    ap.add_argument("--privacy", default="public", choices=["private", "unlisted", "public"])
    args = ap.parse_args()

    creds = get_creds(args.token)
    youtube = build("youtube", "v3", credentials=creds)

    mine = youtube.channels().list(part="id,snippet", mine=True).execute()
    ch = (mine.get("items") or [None])[0]
    if not ch:
        print("ERROR: authorized account has no YouTube channel")
        return 2
    print(f"Authorized channel: {ch['snippet']['title']} ({ch['id']})")

    body = {
        "snippet": {
            "title": TITLE[:100],
            "description": DESCRIPTION,
            "tags": TAGS,
            "categoryId": "28",  # Science & Technology
        },
        "status": {
            "privacyStatus": args.privacy,
            "selfDeclaredMadeForKids": False,
        },
    }
    media = MediaFileUpload(str(args.video), mimetype="video/mp4", resumable=True, chunksize=8 * 1024 * 1024)
    request = youtube.videos().insert(part="snippet,status", body=body, media_body=media)
    response = None
    while response is None:
        status, response = request.next_chunk()
        if status:
            print(f"  … {int(status.progress() * 100)}%", flush=True)

    vid = response["id"]
    url = f"https://youtu.be/{vid}"
    print(f"OK {url}")
    print(json.dumps({"id": vid, "url": url, "title": response["snippet"]["title"]}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
