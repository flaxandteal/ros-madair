# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Flax & Teal Limited

"""RosMadair — page-based static SPARQL query engine for alizarin data."""

from ros_madair.ros_madair import IndexBuilder, IndexReader

__all__ = ["IndexBuilder", "IndexReader"]

# Optional: register as alizarin query provider if available
try:
    import alizarin
    alizarin.register_query_provider("ros-madair", IndexBuilder)
except (ImportError, AttributeError):
    pass
