"""`python -m querymt_harbor.rollout ...` is `openenv harbor rollout ...` plus `querymt`."""

from ._delegate import delegate

if __name__ == "__main__":
    delegate("rollout")
