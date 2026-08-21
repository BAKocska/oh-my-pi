use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{
	Expr, ExprLit, FnArg, GenericArgument, Ident, ItemFn, LitBool, LitInt, LitStr, Meta, Pat,
	PathArguments, ReturnType, Token, Type, parse::Parser, punctuated::Punctuated,
};

struct Args {
	size:   usize,
	result: bool,
	name:   Option<Ident>,
}

pub fn expand(attributes: TokenStream, item: TokenStream) -> syn::Result<TokenStream> {
	let args = parse_args(attributes)?;
	let function: ItemFn = syn::parse2(item)?;
	if function.sig.asyncness.is_some() {
		return Err(syn::Error::new_spanned(
			function.sig.asyncness,
			"cached functions must be synchronous",
		));
	}
	if !function.sig.generics.params.is_empty() || function.sig.generics.where_clause.is_some() {
		return Err(syn::Error::new_spanned(
			&function.sig.generics,
			"cached functions must not be generic",
		));
	}

	let mut arg_names = Vec::with_capacity(function.sig.inputs.len());
	let mut arg_types = Vec::with_capacity(function.sig.inputs.len());
	for argument in &function.sig.inputs {
		let FnArg::Typed(argument) = argument else {
			return Err(syn::Error::new_spanned(argument, "cached cannot be used on methods"));
		};
		let Pat::Ident(pattern) = argument.pat.as_ref() else {
			return Err(syn::Error::new_spanned(
				&argument.pat,
				"cached function arguments must be identifiers",
			));
		};
		if pattern.by_ref.is_some() || pattern.subpat.is_some() {
			return Err(syn::Error::new_spanned(
				pattern,
				"cached function arguments must be plain identifiers",
			));
		}
		arg_names.push(pattern.ident.clone());
		arg_types.push((*argument.ty).clone());
	}

	let ReturnType::Type(_, return_type) = &function.sig.output else {
		return Err(syn::Error::new_spanned(
			&function.sig.output,
			"cached functions must return a value",
		));
	};
	let value_type = if args.result {
		result_ok_type(return_type)?
	} else {
		return_type.as_ref().clone()
	};

	let attributes = &function.attrs;
	let visibility = &function.vis;
	let signature = &function.sig;
	let function_name = &signature.ident;
	let inner_name = format_ident!("__omp_cached_inner_{function_name}");
	let cache_name = args
		.name
		.unwrap_or_else(|| format_ident!("{}_CACHE", function_name.to_string().to_ascii_uppercase()));
	let block = &function.block;
	let size = args.size;
	let key_type = quote!((#(#arg_types,)*));
	let key_value = quote!((#(#arg_names.clone(),)*));
	let inner_signature = {
		let mut inner = signature.clone();
		inner.ident = inner_name.clone();
		inner
	};

	let body = if args.result {
		quote! {
			if let Some(__omp_cached_value) = #cache_name.with(|__omp_cached_cache| {
				__omp_cached_cache.borrow_mut().get_cloned(&__omp_cached_key)
			}) {
				return ::core::result::Result::Ok(__omp_cached_value);
			}
			let __omp_cached_result = #inner_name(#(#arg_names),*);
			if let ::core::result::Result::Ok(__omp_cached_value) = &__omp_cached_result {
				#cache_name.with(|__omp_cached_cache| {
					__omp_cached_cache
						.borrow_mut()
						.insert(__omp_cached_key, __omp_cached_value.clone());
				});
			}
			__omp_cached_result
		}
	} else {
		quote! {
			if let Some(__omp_cached_value) = #cache_name.with(|__omp_cached_cache| {
				__omp_cached_cache.borrow_mut().get_cloned(&__omp_cached_key)
			}) {
				return __omp_cached_value;
			}
			let __omp_cached_value = #inner_name(#(#arg_names),*);
			#cache_name.with(|__omp_cached_cache| {
				__omp_cached_cache
					.borrow_mut()
					.insert(__omp_cached_key, __omp_cached_value.clone());
			});
			__omp_cached_value
		}
	};

	Ok(quote! {
		#(#attributes)*
		#visibility #signature {
			#inner_signature #block
			::std::thread_local! {
				static #cache_name: ::std::cell::RefCell<
					::omp_core::MemoCache<#key_type, #value_type>
				> = ::std::cell::RefCell::new(::omp_core::MemoCache::new(#size));
			}
			let __omp_cached_key = #key_value;
			#body
		}
	})
}

fn parse_args(attributes: TokenStream) -> syn::Result<Args> {
	let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
	let attributes = parser.parse2(attributes)?;
	let mut size = None;
	let mut result = false;
	let mut saw_result = false;
	let mut name = None;

	for attribute in attributes {
		let Meta::NameValue(attribute) = attribute else {
			return Err(syn::Error::new_spanned(
				attribute,
				"expected `size = <integer>`, `result = <bool>`, or `name = \"IDENT\"`",
			));
		};
		if attribute.path.is_ident("size") {
			if size.is_some() {
				return Err(syn::Error::new_spanned(attribute, "duplicate `size` argument"));
			}
			let literal: LitInt = expression_literal(&attribute.value)?;
			size = Some(literal.base10_parse()?);
		} else if attribute.path.is_ident("result") {
			if saw_result {
				return Err(syn::Error::new_spanned(attribute, "duplicate `result` argument"));
			}
			let literal: LitBool = expression_literal(&attribute.value)?;
			result = literal.value;
			saw_result = true;
		} else if attribute.path.is_ident("name") {
			if name.is_some() {
				return Err(syn::Error::new_spanned(attribute, "duplicate `name` argument"));
			}
			let literal: LitStr = expression_literal(&attribute.value)?;
			name = Some(literal.parse()?);
		} else {
			return Err(syn::Error::new_spanned(attribute.path, "unknown `cached` argument"));
		}
	}

	let size = size
		.ok_or_else(|| syn::Error::new(proc_macro2::Span::call_site(), "missing `size` argument"))?;
	Ok(Args { size, result, name })
}

fn expression_literal<T: syn::parse::Parse>(expression: &Expr) -> syn::Result<T> {
	let Expr::Lit(ExprLit { lit, .. }) = expression else {
		return Err(syn::Error::new_spanned(expression, "expected a literal"));
	};
	syn::parse2(quote!(#lit))
}

fn result_ok_type(return_type: &Type) -> syn::Result<Type> {
	let Type::Path(path) = return_type else {
		return Err(syn::Error::new_spanned(
			return_type,
			"`result = true` requires a Result return type",
		));
	};
	let Some(segment) = path.path.segments.last() else {
		return Err(syn::Error::new_spanned(
			return_type,
			"`result = true` requires a Result return type",
		));
	};
	if segment.ident != "Result" {
		return Err(syn::Error::new_spanned(
			return_type,
			"`result = true` requires a Result return type",
		));
	}
	let PathArguments::AngleBracketed(arguments) = &segment.arguments else {
		return Err(syn::Error::new_spanned(
			return_type,
			"`result = true` requires a Result return type",
		));
	};
	let Some(GenericArgument::Type(ok_type)) = arguments.args.first() else {
		return Err(syn::Error::new_spanned(return_type, "Result must have an Ok type"));
	};
	Ok(ok_type.clone())
}
