"""`python -m querymt_harbor.serve ...` == `openenv harbor serve ...` plus the querymt harness."""

from ._delegate import delegate

if __name__ == "__main__":
    delegate("serve")
