from flask import Flask, request

app = Flask(__name__)


@app.route("/api/flask_app", methods=["GET", "POST"])
def index():
    return {"function": "api/flask_app.py", "method": request.method, "args": request.args, "form": request.form}
