"""Analytic two-level Rabi sweep: infrastructure smoke test, NOT a Heron twin.

Each Job index selects one parameter tuple. Results are keyed by Job UID and
index, so retries reuse an atomically committed result. A new Job UID produces
new results. Replace the calculation with the REU solver once validated.
"""
import itertools
import json
import math
import os
from pathlib import Path

PARAMETERS = list(itertools.product([0.005, 0.01, 0.02], [20, 40, 80]))


def main():
    index = int(os.environ["RK_JOB_INDEX"])
    attempt = int(os.environ["RK_JOB_ATTEMPT"])
    job_uid = os.environ["RK_JOB_UID"]
    if not job_uid.isalnum():
        raise ValueError("invalid Job UID")
    rabi_ghz, duration_ns = PARAMETERS[index]
    output = Path(os.environ.get("RK_RESULTS_DIR", "/results")) / job_uid
    output.mkdir(parents=True, exist_ok=True)
    final = output / f"{index}.json"
    if final.exists():
        print(f"Index {index}: result already committed", flush=True)
        return
    result = {
        "model": "ideal-resonant-two-level-rabi",
        "job_uid": job_uid,
        "index": index,
        "attempt": attempt,
        "rabi_frequency_ghz": rabi_ghz,
        "duration_ns": duration_ns,
        "excited_probability": math.sin(math.pi * rabi_ghz * duration_ns) ** 2,
    }
    temporary = output / f".{index}.{attempt}.{os.getpid()}.tmp"
    with temporary.open("w") as handle:
        json.dump(result, handle, indent=2)
        handle.write("\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, final)
    directory = os.open(output, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
