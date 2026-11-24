"""An optional model budget for each role, which `--arg budget_microusd=<n>` sets."""

# Millionths of a US dollar, as `USD` counts them; 0, the default, sets no budget.
BUDGET_MICROUSD = arg("budget_microusd", int, default = 0)

def budgeted(routes):
    """Gives each entry of a role's model route the budget, if one is set.

    Args:
      routes: the entries of the role's model route.

    Returns:
      The entries, each with the budget, or as they are without one.
    """
    if BUDGET_MICROUSD == 0:
        return routes
    return [route | {"budget": {"microusd": BUDGET_MICROUSD}} for route in routes]
