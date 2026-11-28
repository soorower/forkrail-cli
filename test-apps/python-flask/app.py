"""A Flask app as Vercel's Flask preset finds it: `app` in app.py, served as WSGI."""

import os
import sys

from flask import Flask, jsonify, request

app = Flask(__name__)


@app.get("/")
def index():
    return jsonify(framework="flask", python=sys.version.split()[0], path=request.path)


@app.route("/echo", methods=["POST"])
def echo():
    print(f"echo {len(request.get_data())} bytes")
    return jsonify(body=request.get_data(as_text=True), headers={"host": request.host})


@app.get("/env/<name>")
def env(name):
    return jsonify(name=name, value=os.environ.get(name))
