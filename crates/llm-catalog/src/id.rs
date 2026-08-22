//! Strongly typed catalog identifiers.

use omp_core::string_id;

string_id!(/// Identifies a commercial, hosted, or local provider domain.
	ProviderId);
string_id!(/// Identifies one concrete provider route.
	RouteId);
string_id!(/// Identifies one wire codec implementation.
	CodecId);
string_id!(/// Identifies one normalized selectable model deployment.
	ModelKey);
string_id!(/// Identifies a normalized model class (vendor lineage).
	ClassId);
string_id!(/// Identifies a product family within a class (for example flash, sonnet, or opus).
	FamilyId);
string_id!(/// Identifies an interned authentication specification.
	AuthSpecId);
string_id!(/// Identifies an interned public OAuth flow specification.
	OAuthSpecId);
string_id!(/// Identifies an interned static header profile.
	HeaderProfileId);
string_id!(/// Identifies an interned model-discovery specification.
	DiscoverySpecId);
string_id!(/// Identifies an interned wire-lowering policy.
	WirePolicyId);
string_id!(/// Identifies an interned reasoning policy.
	ThinkingPolicyId);
string_id!(/// Carries the opaque model identifier expected by a wire endpoint.
	WireModelId);
string_id!(/// Identifies an immutable catalog revision.
	CatalogRevision);
