import random
from dataclasses import dataclass


@dataclass
class Point:
    x: int


p = Point(random.randint(1, 1))
print(p.x)
