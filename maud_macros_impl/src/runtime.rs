extern crate alloc;
use alloc::string::String;

use proc_macro2::{Ident, Span, TokenStream, TokenTree};
use quote::quote;

use crate::expand;
use crate::generate::desugar_attrs;
use crate::{ast::*, escape, expand_from_parsed, expand_runtime_from_parsed};

pub fn generate(vars_ident: Option<TokenTree>, markups: Vec<Markup>) -> TokenStream {
    let mut build = RuntimeBuilder::new(vars_ident.clone());
    RuntimeGenerator::new().markups(markups, &mut build);
    build.finish()
}

pub fn format_str(vars_ident: Option<TokenTree>, markups: Vec<Markup>) -> String {
    let mut build = RuntimeBuilder::new(vars_ident.clone());
    RuntimeGenerator::new().markups(markups, &mut build);
    build.format_str()
}

struct RuntimeGenerator {}

impl RuntimeGenerator {
    fn new() -> RuntimeGenerator {
        RuntimeGenerator {}
    }

    fn markups(&self, markups: Vec<Markup>, build: &mut RuntimeBuilder) {
        for markup in markups {
            self.markup(markup, build);
        }
    }

    fn markup(&self, markup: Markup, build: &mut RuntimeBuilder) {
        match markup {
            Markup::ParseError { .. } => {}
            Markup::Block(Block {
                markups,
                outer_span,
                raw_body,
            }) => {
                if markups
                    .iter()
                    .any(|markup| matches!(*markup, Markup::Let { .. }))
                {
                    self.block(
                        Block {
                            markups,
                            outer_span,
                            raw_body,
                        },
                        build,
                    );
                } else {
                    self.markups(markups, build);
                }
            }
            Markup::Literal { content, .. } => build.push_escaped(&content),
            Markup::Symbol { symbol } => self.name(symbol, build),
            Markup::Splice { expr, .. } => self.splice(expr, build),
            Markup::Element { name, attrs, body } => self.element(name, attrs, body, build),
            Markup::Let { tokens, .. } => {
                // this is a bit dicey
                build.tokens.extend(tokens);
            }
            Markup::Special { segments, .. } => self.special(segments, build),
            // fallback case: use static generator to render a subset of the template
            markup => {
                let tt = expand_from_parsed(vec![markup], 0);

                build.push_format_arg(tt);
            }
        }
    }

    fn block(&self, block: Block, build: &mut RuntimeBuilder) {
        self.special(
            vec![Special {
                at_span: block.outer_span,
                head: quote!(),
                body: block,
            }],
            build,
        );
    }

    fn special(&self, segments: Vec<Special>, build: &mut RuntimeBuilder) {
        let output_ident =
            TokenTree::Ident(Ident::new("__maud_special_output", Span::mixed_site()));
        let mut tt = TokenStream::new();
        for Special { head, body, .. } in segments {
            let body = if let Some(raw_body) = body.raw_body {
                expand_runtime_from_parsed(raw_body, body.markups, &head.to_string())
            } else {
                expand_from_parsed(body.markups, 0)
            };
            tt.extend(quote! {
                #head {
                    ::maud::Render::render_to(&#body, &mut #output_ident);
                }
            });
        }
        build.push_format_arg(quote! {{
            extern crate maud;
            let mut #output_ident = ::maud::macro_private::String::new();
            #tt
            ::maud::PreEscaped(#output_ident)
        }});
    }

    fn splice(&self, expr: TokenStream, build: &mut RuntimeBuilder) {
        build.push_format_arg(expr);
    }

    fn element(
        &self,
        name: TokenStream,
        attrs: Vec<Attr>,
        body: ElementBody,
        build: &mut RuntimeBuilder,
    ) {
        build.push_str("<");
        self.name(name.clone(), build);
        self.attrs(attrs, build);
        build.push_str(">");
        if let ElementBody::Block { block } = body {
            self.markups(block.markups, build);
            build.push_str("</");
            self.name(name, build);
            build.push_str(">");
        }
    }

    fn name(&self, name: TokenStream, build: &mut RuntimeBuilder) {
        build.push_escaped(&name_to_string(name));
    }

    fn attrs(&self, attrs: Vec<Attr>, build: &mut RuntimeBuilder) {
        for NamedAttr { name, attr_type } in desugar_attrs(attrs) {
            match attr_type {
                AttrType::Normal { value } => {
                    build.push_str(" ");
                    self.name(name, build);
                    build.push_str("=\"");
                    self.markup(value, build);
                    build.push_str("\"");
                }
                AttrType::Optional {
                    toggler: Toggler { cond, .. },
                } => {
                    let inner_value = quote!(inner_value);
                    let name_tok = name_to_string(name);
                    let body = expand(quote! {
                        (::maud::PreEscaped(" "))
                        (#name_tok)
                        (::maud::PreEscaped("=\""))
                        (#inner_value)
                        (::maud::PreEscaped("\""))
                    });

                    build.push_format_arg(quote! {
                        if let Some(#inner_value) = (#cond) {
                            #body
                        } else {
                            ::maud::PreEscaped("".to_owned())
                        }
                    });
                }
                AttrType::Empty { toggler: None } => {
                    build.push_str(" ");
                    self.name(name, build);
                }
                AttrType::Empty {
                    toggler: Some(Toggler { cond, .. }),
                } => {
                    let name_tok = name_to_string(name);
                    let body = expand(quote! {
                        " "
                        (#name_tok)
                    });

                    build.push_format_arg(quote! {
                        if (#cond) {
                            #body
                        } else {
                            ::maud::PreEscaped("".to_owned())
                        }
                    });
                }
            }
        }
    }
}

////////////////////////////////////////////////////////

struct RuntimeBuilder {
    vars_ident: Option<TokenTree>,
    tokens: Vec<TokenTree>,
    format_str: String,
    arg_track: u32,
}

impl RuntimeBuilder {
    fn new(vars_ident: Option<TokenTree>) -> RuntimeBuilder {
        RuntimeBuilder {
            vars_ident,
            tokens: Vec::new(),
            format_str: String::new(),
            arg_track: 0,
        }
    }

    fn push_str(&mut self, string: &str) {
        self.format_str.push_str(string);
    }

    fn push_escaped(&mut self, string: &str) {
        // escape for leon templating. the string itself cannot contain raw {} otherwise
        let string = string
            .replace(r"\", r"\\")
            .replace(r"{", r"\{")
            .replace(r"}", r"\}");
        escape::escape_to_string(&string, &mut self.format_str);
    }

    fn push_format_arg(&mut self, expr: TokenStream) {
        let arg_track = self.arg_track.to_string();

        if let Some(ref vars) = self.vars_ident {
            self.tokens.extend(quote! {
                #vars.insert(#arg_track, {
                    extern crate maud;
                    let mut buf = ::maud::macro_private::String::new();
                    ::maud::macro_private::render_to!(&(#expr), &mut buf);
                    buf
                });
            });
        }

        self.arg_track = self.arg_track + 1;
        self.format_str.push_str(&format!("{{{}}}", arg_track));
    }

    fn format_str(&self) -> String {
        self.format_str.clone()
    }

    fn finish(self) -> TokenStream {
        self.tokens.into_iter().collect::<TokenStream>()
    }
}
