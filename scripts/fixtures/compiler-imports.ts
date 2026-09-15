// The string and comment below must not be treated as imports.
const fake = "import('./false-positive')";
/* require("./commented-out") */

import {
  value,
} from "./multiline";
import { type NamedType } from "./named-type";
import { type MixedType, mixedValue } from "./mixed-type";
export { value as reExported } from "./re-export";
type Shape = import("./types").Shape;
const lazy = import("./lazy");
let conditional: Promise<unknown> | undefined;
if (getCondition()) conditional = import("./conditional");
const dynamic = import(getSpecifier());
const required = require("./required");
const dynamicRequired = require(getSpecifier());

export { lazy, required, dynamic, dynamicRequired };
const named: NamedType | undefined = undefined;
const mixed: MixedType | undefined = mixedValue ? undefined : undefined;
