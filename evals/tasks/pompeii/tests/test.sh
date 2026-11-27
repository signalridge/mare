#!/bin/bash
# shared verifier: runs in the agent's container, from the working directory (/app)
python3 /tests/check.py
