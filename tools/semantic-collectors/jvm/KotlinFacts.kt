import com.intellij.psi.PsiElement
import java.io.File
import java.io.OutputStream
import java.io.PrintStream
import java.nio.file.Path
import org.jetbrains.kotlin.KtFakeSourceElementKind
import org.jetbrains.kotlin.KtPsiSourceElement
import org.jetbrains.kotlin.KtRealSourceElementKind
import org.jetbrains.kotlin.KtSourceElement
import org.jetbrains.kotlin.cli.common.ExitCode
import org.jetbrains.kotlin.cli.common.messages.MessageRenderer
import org.jetbrains.kotlin.cli.common.messages.PrintingMessageCollector
import org.jetbrains.kotlin.cli.jvm.K2JVMCompiler
import org.jetbrains.kotlin.compiler.plugin.CompilerPluginRegistrar
import org.jetbrains.kotlin.compiler.plugin.ExperimentalCompilerApi
import org.jetbrains.kotlin.config.CompilerConfiguration
import org.jetbrains.kotlin.config.JvmTarget
import org.jetbrains.kotlin.config.Services
import org.jetbrains.kotlin.descriptors.Modality
import org.jetbrains.kotlin.diagnostics.DiagnosticReporter
import org.jetbrains.kotlin.fir.FirSession
import org.jetbrains.kotlin.fir.unwrapUseSiteSubstitutionOverrides
import org.jetbrains.kotlin.fir.analysis.checkers.MppCheckerKind
import org.jetbrains.kotlin.fir.analysis.checkers.context.CheckerContext
import org.jetbrains.kotlin.fir.analysis.checkers.declaration.DeclarationCheckers
import org.jetbrains.kotlin.fir.analysis.checkers.declaration.FirFileChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.ExpressionCheckers
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirCallChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirEqualityOperatorCallChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirGetClassCallChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirPropertyAccessExpressionChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirThisReceiverExpressionChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirQualifiedAccessExpressionChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirResolvedQualifierChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirVariableAssignmentChecker
import org.jetbrains.kotlin.fir.analysis.checkers.type.FirResolvedTypeRefChecker
import org.jetbrains.kotlin.fir.analysis.checkers.type.TypeCheckers
import org.jetbrains.kotlin.fir.analysis.extensions.FirAdditionalCheckersExtension
import org.jetbrains.kotlin.fir.declarations.FirFile
import org.jetbrains.kotlin.fir.declarations.FirResolvedImport
import org.jetbrains.kotlin.fir.declarations.processAllDeclaredCallables
import org.jetbrains.kotlin.fir.expressions.FirCall
import org.jetbrains.kotlin.fir.expressions.FirDesugaredAssignmentValueReferenceExpression
import org.jetbrains.kotlin.fir.expressions.FirEqualityOperatorCall
import org.jetbrains.kotlin.fir.expressions.FirFunctionCall
import org.jetbrains.kotlin.fir.expressions.FirGetClassCall
import org.jetbrains.kotlin.fir.expressions.FirResolvedReifiedParameterReference
import org.jetbrains.kotlin.fir.expressions.FirThisReceiverExpression
import org.jetbrains.kotlin.fir.expressions.FirImplicitInvokeCall
import org.jetbrains.kotlin.fir.expressions.FirOperation
import org.jetbrains.kotlin.fir.expressions.FirPropertyAccessExpression
import org.jetbrains.kotlin.fir.expressions.FirQualifiedAccessExpression
import org.jetbrains.kotlin.fir.expressions.FirResolvable
import org.jetbrains.kotlin.fir.expressions.FirResolvedQualifier
import org.jetbrains.kotlin.fir.expressions.FirVariableAssignment
import org.jetbrains.kotlin.fir.expressions.UnresolvedExpressionTypeAccess
import org.jetbrains.kotlin.fir.expressions.impl.FirResolvedArgumentList
import org.jetbrains.kotlin.fir.extensions.FirExtensionRegistrar
import org.jetbrains.kotlin.fir.extensions.FirExtensionRegistrarAdapter
import org.jetbrains.kotlin.fir.references.FirResolvedErrorReference
import org.jetbrains.kotlin.fir.references.FirResolvedNamedReference
import org.jetbrains.kotlin.fir.resolve.providers.symbolProvider
import org.jetbrains.kotlin.fir.resolve.toSymbol
import org.jetbrains.kotlin.fir.resolve.typeParameterSymbol
import org.jetbrains.kotlin.fir.scopes.impl.typeAliasConstructorInfo
import org.jetbrains.kotlin.fir.symbols.FirBasedSymbol
import org.jetbrains.kotlin.fir.symbols.impl.FirCallableSymbol
import org.jetbrains.kotlin.fir.symbols.impl.FirConstructorSymbol
import org.jetbrains.kotlin.fir.symbols.impl.FirPropertySymbol
import org.jetbrains.kotlin.fir.symbols.impl.FirRegularClassSymbol
import org.jetbrains.kotlin.fir.symbols.impl.FirNamedFunctionSymbol
import org.jetbrains.kotlin.fir.symbols.impl.FirReceiverParameterSymbol
import org.jetbrains.kotlin.fir.types.ConeClassLikeType
import org.jetbrains.kotlin.fir.types.ConeErrorType
import org.jetbrains.kotlin.fir.types.ConeFlexibleType
import org.jetbrains.kotlin.fir.types.ConeTypeParameterType
import org.jetbrains.kotlin.fir.types.FirResolvedTypeRef
import org.jetbrains.kotlin.fir.types.isBoolean
import org.jetbrains.kotlin.fir.types.isNullableAny
import org.jetbrains.kotlin.fir.types.isNullableNothing
import org.jetbrains.kotlin.name.ClassId
import org.jetbrains.kotlin.psi.KtBinaryExpression
import org.jetbrains.kotlin.psi.KtClassLiteralExpression
import org.jetbrains.kotlin.psi.KtConstructor
import org.jetbrains.kotlin.psi.KtThisExpression
import org.jetbrains.kotlin.psi.KtDotQualifiedExpression
import org.jetbrains.kotlin.psi.KtImportDirective
import org.jetbrains.kotlin.psi.KtNameReferenceExpression
import org.jetbrains.kotlin.psi.KtParameter
import org.jetbrains.kotlin.psi.KtPropertyAccessor
import org.jetbrains.kotlin.psi.KtTypeReference
import org.jetbrains.kotlin.psi.KtUserType
import org.jetbrains.kotlin.psi.KtValueArgument
import org.jetbrains.kotlin.util.OperatorNameConventions

class KotlinFacts : KotlinCompiler {
    override fun collect(
        facts: Facts, paths: List<Path>, classpath: List<Path>, home: Path, output: Path,
        stdlib: Boolean, settings: KotlinLanguageSettings,
    ) {
        val options = mutableListOf(
            "-kotlin-home", home.toString(), "-d", output.toString(),
            "-Xuse-fir-lt=false", "-Xdisable-default-scripting-plugin", "-Xuse-fast-jar-file-system=false",
            "-Xplugin=${Path.of(KotlinFacts::class.java.protectionDomain.codeSource.location.toURI())}",
        )
        if (classpath.isNotEmpty()) options.addAll(listOf("-classpath", classpath.joinToString(File.pathSeparator)))
        settings.optIns().forEach { options.add("-opt-in=$it") }
        settings.languageVersion()?.let { options.addAll(listOf("-language-version", it)) }
        settings.apiVersion()?.let { options.addAll(listOf("-api-version", it)) }
        settings.jvmTarget()?.let { options.addAll(listOf("-jvm-target", it)) }
        val friends = settings.friendPaths().toMutableList()
        if (stdlib) {
            options.add("-Xallow-kotlin-package")
            if (settings.jvmTarget() == null || settings.jvmTarget() == JvmTarget.JVM_1_8.description) {
                options.add("-Xjdk-release=8")
            }
            friends.add(home.resolve("lib").resolve("kotlin-stdlib.jar"))
        }
        options.addAll(paths.map(Path::toString))
        val compiler = K2JVMCompiler()
        val arguments = compiler.createArguments()
        compiler.parseArguments(options.toTypedArray(), arguments)
        if (friends.isNotEmpty()) arguments.friendPaths = friends.map(Path::toString).toTypedArray()

        val active = KotlinFactsRegistrar.facts
        check(active.get() == null)
        active.set(facts)
        try {
            PrintStream(OutputStream.nullOutputStream()).use { diagnostics ->
                val messages = PrintingMessageCollector(diagnostics, MessageRenderer.PLAIN_FULL_PATHS, false)
                val exit = compiler.exec(messages, Services.EMPTY, arguments)
                facts.partial = facts.partial || exit != ExitCode.OK
            }
        } finally {
            active.remove()
        }
    }
}

@OptIn(ExperimentalCompilerApi::class)
class KotlinFactsRegistrar : CompilerPluginRegistrar() {
    override val supportsK2 = true
    override val pluginId = "rift.jvm.collector"

    override fun ExtensionStorage.registerExtensions(configuration: CompilerConfiguration) {
        FirExtensionRegistrarAdapter.registerExtension(Registrar(checkNotNull(facts.get())))
    }

    private class Registrar(val facts: Facts) : FirExtensionRegistrar() {
        override fun ExtensionRegistrarContext.configurePlugin() {
            +{ session: FirSession -> KotlinCheckers(session, facts) }
        }
    }

    companion object {
        internal val facts = ThreadLocal<Facts>()
    }
}

private class KotlinCheckers(session: FirSession, val facts: Facts) :
    FirAdditionalCheckersExtension(session) {

    private fun KtSourceElement?.range(): Facts.Range? {
        val source = this ?: return null
        return if (source.kind == KtRealSourceElementKind && source.startOffset >= 0 &&
            source.endOffset > source.startOffset
        ) Facts.Range(source.startOffset, source.endOffset) else null
    }

    private fun PsiElement?.range(): Facts.Range? {
        val range = this?.textRange ?: return null
        return if (range.isEmpty) null else Facts.Range(range.startOffset, range.endOffset)
    }

    private fun PsiElement?.nameRange(): Facts.Range? {
        val name = when (this) {
            is KtNameReferenceExpression -> this
            is KtDotQualifiedExpression -> selectorExpression as? KtNameReferenceExpression
            else -> null
        }
        return name?.getReferencedNameElement().range()
    }

    private fun FirBasedSymbol<*>?.anchor(): Facts.Anchor? {
        val canonical = (this as? FirCallableSymbol<*>)?.unwrapUseSiteSubstitutionOverrides() ?: this
        val source = canonical?.source as? KtPsiSourceElement ?: return null
        val file = source.psi.containingFile?.virtualFile ?: return null
        val id = facts.source(Path.of(file.path)) ?: return null
        val declaration = source.psi
        val range = when {
            canonical is FirConstructorSymbol && declaration is KtConstructor<*> ->
                declaration.getConstructorKeyword().range()
                    ?: declaration.getContainingClassOrObject().nameIdentifier.range()
            source.kind == KtFakeSourceElementKind.PropertyFromParameter ->
                // A property declared with val or var in the constructor shares the parameter's name.
                (declaration as? KtParameter)?.takeIf { it.hasValOrVar() }?.nameIdentifier.range()
            else -> source.range()
        }
        return range?.let { Facts.Anchor(id, it) }
    }

    private fun CheckerContext.isSelected() =
        containingFilePath?.let { facts.source(Path.of(it)) == 0 } == true

    private fun CheckerContext.enclosing() =
        containingDeclarations.asReversed().firstNotNullOfOrNull { symbol ->
            symbol.source.range()
        }

    private fun FirQualifiedAccessExpression.referenceRange(): Facts.Range? {
        val reference = calleeReference.source
        reference.range()?.let { return it }
        val expression = source as? KtPsiSourceElement ?: return null
        val name = expression.psi as? KtNameReferenceExpression ?: return null
        if (expression.kind == KtFakeSourceElementKind.DesugaredPlusAssign &&
            (name.parent as? KtBinaryExpression)?.left === name) {
            return name.getReferencedNameElement().range()
        }
        // FIR gives an authored atomic name a fake reference range and a real expression range.
        return expression.takeIf { reference?.kind == KtFakeSourceElementKind.ReferenceInAtomicQualifiedAccess }.range()
    }

    @OptIn(UnresolvedExpressionTypeAccess::class)
    private fun equalityTarget(expression: FirEqualityOperatorCall): Facts.Anchor? {
        if (expression.operation != FirOperation.EQ && expression.operation != FirOperation.NOT_EQ) return null
        val operands = expression.argumentList.arguments
        val receiver = operands.getOrNull(0)?.coneTypeOrNull as? ConeClassLikeType ?: return null
        val argument = operands.getOrNull(1)?.coneTypeOrNull ?: return null
        if (receiver is ConeErrorType || argument is ConeErrorType || argument.isNullableNothing) return null
        val owner = receiver.lookupTag.toSymbol(session) as? FirRegularClassSymbol ?: return null
        if (owner.resolvedStatus.modality != Modality.FINAL) return null
        val matches = buildList {
            owner.processAllDeclaredCallables(session) {
                if (it is FirNamedFunctionSymbol && it.name == OperatorNameConventions.EQUALS &&
                    it.resolvedStatus.isOverride && it.resolvedReturnType.isBoolean &&
                    it.valueParameterSymbols.singleOrNull()?.resolvedReturnType?.isNullableAny == true) {
                    add(it)
                }
            }
        }
        return matches.singleOrNull().anchor()
    }

    override val expressionCheckers = object : ExpressionCheckers() {
        override val equalityOperatorCallCheckers = setOf(
            object : FirEqualityOperatorCallChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirEqualityOperatorCall) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val source = expression.source as? KtPsiSourceElement ?: return
                    if (source.kind != KtRealSourceElementKind) return
                    val operator = (source.psi as? KtBinaryExpression)?.operationReference ?: return
                    val occurrence = operator.getReferencedNameElement().range() ?: return
                    val target = equalityTarget(expression) ?: return
                    facts.add(occurrence, context.enclosing(), target)
                }
            }
        )

        override val callCheckers = setOf(
            object : FirCallChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirCall) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val reference = (expression as? FirResolvable)?.calleeReference
                    if (reference !is FirResolvedNamedReference || reference is FirResolvedErrorReference) return
                    val args = expression.argumentList as? FirResolvedArgumentList ?: return
                    for ((argument, parameter) in args.mapping) {
                        val source = argument.source as? KtPsiSourceElement ?: continue
                        if (source.kind != KtRealSourceElementKind && source.kind != KtFakeSourceElementKind.VarargArgument) continue
                        val psi = source.psi
                        val syntax = psi as? KtValueArgument ?: psi.parent as? KtValueArgument ?: continue
                        val name = syntax.getArgumentName()?.referenceExpression ?: continue
                        if (name.getReferencedNameAsName() != parameter.name) continue
                        val occurrence = name.getReferencedNameElement().range() ?: continue
                        if ((parameter.source as? KtPsiSourceElement)?.psi !is KtParameter) continue
                        val target = parameter.symbol.anchor() ?: continue
                        facts.add(occurrence, context.enclosing(), target)
                    }
                }
            }
        )

        override val getClassCallCheckers = setOf(
            object : FirGetClassCallChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirGetClassCall) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val source = expression.source as? KtPsiSourceElement ?: return
                    if (source.kind != KtRealSourceElementKind) return
                    val literal = source.psi as? KtClassLiteralExpression ?: return
                    val argument = expression.argument as? FirResolvedReifiedParameterReference ?: return
                    val occurrence = literal.receiverExpression.nameRange() ?: return
                    facts.add(occurrence, context.enclosing(), argument.symbol.anchor())
                }
            }
        )

        override val thisReceiverExpressionCheckers = setOf(
            object : FirThisReceiverExpressionChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirThisReceiverExpression) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val source = expression.source as? KtPsiSourceElement ?: return
                    if (source.kind != KtRealSourceElementKind) return
                    val label = (source.psi as? KtThisExpression)?.getTargetLabel() ?: return
                    val receiver = expression.calleeReference.boundSymbol as? FirReceiverParameterSymbol ?: return
                    val symbol = receiver.containingDeclarationSymbol as? FirNamedFunctionSymbol ?: return
                    if (symbol.name != label.getReferencedNameAsName()) return
                    val occurrence = label.getReferencedNameElement().range() ?: return
                    facts.add(occurrence, context.enclosing(), symbol.anchor())
                }
            }
        )

        override val resolvedQualifierCheckers = setOf(
            object : FirResolvedQualifierChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirResolvedQualifier) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val classifier = expression.qualifierSymbol ?: return
                    val source = expression.source as? KtPsiSourceElement ?: return
                    if (source.kind != KtRealSourceElementKind) return
                    val occurrence = source.psi.nameRange() ?: return
                    val owner = context.enclosing()
                    facts.add(occurrence, owner, classifier.anchor())
                    if (expression.resolvedToCompanionObject) {
                        (classifier as? FirRegularClassSymbol)?.resolvedCompanionObjectSymbol
                            .anchor()?.let { facts.add(occurrence, owner, it) }
                    }
                }
            }
        )

        override val variableAssignmentCheckers = setOf(
            object : FirVariableAssignmentChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirVariableAssignment) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val wrapper = expression.lValue as? FirDesugaredAssignmentValueReferenceExpression ?: return
                    val access = wrapper.expressionRef.value as? FirQualifiedAccessExpression ?: return
                    val reference = access.calleeReference as? FirResolvedNamedReference ?: return
                    val source = (access.source as? KtPsiSourceElement)?.psi
                    val occurrence = access.referenceRange() ?: source.nameRange() ?: return
                    facts.add(occurrence, context.enclosing(), reference.resolvedSymbol.anchor())
                }
            }
        )

        override val propertyAccessExpressionCheckers = setOf(
            object : FirPropertyAccessExpressionChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirPropertyAccessExpression) {
                    if (facts.category != Facts.Category.CALLS || !context.isSelected()) return
                    val reference = expression.calleeReference as? FirResolvedNamedReference ?: return
                    if (reference is FirResolvedErrorReference) return
                    val property = reference.resolvedSymbol as? FirPropertySymbol ?: return
                    if (property.isVar || context.callsOrAssignments.any {
                            it is FirVariableAssignment && it.lValue === expression
                        }) return
                    val getter = property.getterSymbol ?: return
                    val source = getter.source as? KtPsiSourceElement ?: return
                    val token = (source.psi as? KtPropertyAccessor)?.namePlaceholder.range() ?: return
                    val target = getter.anchor() ?: return
                    val occurrence = expression.referenceRange() ?: return
                    facts.add(occurrence, context.enclosing(), Facts.Anchor(target.source(), token))
                }
            }
        )

        override val qualifiedAccessExpressionCheckers = setOf(
            object : FirQualifiedAccessExpressionChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirQualifiedAccessExpression) {
                    if (!context.isSelected()) return
                    val reference = expression.calleeReference
                    val occurrence = when (facts.category) {
                        Facts.Category.CALLS -> if (expression is FirFunctionCall) expression.source.range() else null
                        Facts.Category.REFERENCES -> expression.referenceRange()
                        Facts.Category.IMPORTS -> null
                    } ?: return
                    val receiver = if (facts.category == Facts.Category.REFERENCES && expression is FirImplicitInvokeCall) {
                        expression.dispatchReceiver
                    } else null
                    val qualifier = (receiver as? FirResolvedQualifier)?.takeIf {
                        it.resolvedToCompanionObject && it.source?.kind == KtFakeSourceElementKind.ImplicitInvokeCall
                    }
                    val invokedValue = (receiver as? FirPropertyAccessExpression)?.calleeReference as? FirResolvedNamedReference
                    val symbol = qualifier?.qualifierSymbol
                        ?: (invokedValue ?: reference as? FirResolvedNamedReference)?.resolvedSymbol
                    val target = if (symbol is FirConstructorSymbol) {
                        val alias = symbol.typeAliasConstructorInfo
                        if (facts.category == Facts.Category.REFERENCES) {
                            alias?.typeAliasSymbol ?: (symbol.resolvedReturnType as? ConeClassLikeType)
                                ?.lookupTag?.toSymbol(session)
                        } else {
                            alias?.originalConstructor?.symbol ?: symbol
                        }
                    } else symbol
                    facts.add(occurrence, context.enclosing(), target.anchor())
                }
            }
        )
    }

    override val typeCheckers = object : TypeCheckers() {
        override val resolvedTypeRefCheckers = setOf(
            object : FirResolvedTypeRefChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(typeRef: FirResolvedTypeRef) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val occurrence = typeRef.source.range() ?: return
                    val type = typeRef.coneType
                    val target = when (type) {
                        is ConeClassLikeType -> session.symbolProvider.getClassLikeSymbolByClassId(type.lookupTag.classId)
                        is ConeTypeParameterType -> type.lookupTag.typeParameterSymbol
                        is ConeFlexibleType -> {
                            val lower = (type.lowerBound as? ConeTypeParameterType)?.lookupTag
                            val upper = (type.upperBound as? ConeTypeParameterType)?.lookupTag
                            lower?.takeIf { it == upper }?.typeParameterSymbol
                        }
                        else -> null
                    }
                    val owner = context.enclosing()
                    facts.add(occurrence, owner, target.anchor())
                    if (type is ConeClassLikeType) {
                        recordTypeQualifiers(typeRef.source, type.lookupTag.classId, owner)
                    }
                }
            }
        )
    }

    private fun recordTypeQualifiers(source: KtSourceElement?, classId: ClassId, owner: Facts.Range?) {
        val type = (source as? KtPsiSourceElement)?.psi as? KtTypeReference ?: return
        var syntax = type.typeElement as? KtUserType ?: return
        var classifier = classId
        while (syntax.referenceExpression?.getReferencedNameAsName() == classifier.shortClassName) {
            syntax = syntax.qualifier ?: return
            classifier = classifier.outerClassId ?: return
            val reference = syntax.referenceExpression ?: return
            if (reference.getReferencedNameAsName() != classifier.shortClassName) return
            val occurrence = reference.getReferencedNameElement().range() ?: return
            val target = session.symbolProvider.getClassLikeSymbolByClassId(classifier)
            facts.add(occurrence, owner, target.anchor())
        }
    }

    override val declarationCheckers = object : DeclarationCheckers() {
        override val fileCheckers = setOf(
            object : FirFileChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(declaration: FirFile) {
                    if (facts.category != Facts.Category.IMPORTS || !context.isSelected()) return
                    for (entry in declaration.imports) {
                        val occurrence = entry.source.range() ?: continue
                        val imported = entry as? FirResolvedImport
                        val starOwner = imported?.resolvedParentClassId?.takeIf { entry.isAllUnder }
                        if (starOwner != null) {
                            val directive = (entry.source as? KtPsiSourceElement)?.psi as? KtImportDirective
                            val name = directive?.importedReference.nameRange() ?: occurrence
                            val target = session.symbolProvider.getClassLikeSymbolByClassId(starOwner)
                            facts.add(name, null, target.anchor())
                            continue
                        }
                        val targets = imported?.takeUnless { it.isAllUnder }
                            ?.let { importTargets(it).mapNotNull { symbol -> symbol.anchor() } }
                            .orEmpty()
                        if (targets.isEmpty()) {
                            facts.add(occurrence, null, null)
                        } else {
                            for (target in targets) facts.add(occurrence, null, target)
                        }
                    }
                }
            }
        )
    }

    private fun importTargets(entry: FirResolvedImport): List<FirBasedSymbol<*>> {
        val name = entry.importedName ?: return emptyList()
        val owner = entry.resolvedParentClassId
        val provider = session.symbolProvider
        val type = owner?.createNestedClassId(name) ?: ClassId(entry.packageFqName, name)
        val classifier = provider.getClassLikeSymbolByClassId(type)
        return when {
            classifier != null -> listOf(classifier)
            owner == null -> provider.getTopLevelCallableSymbols(entry.packageFqName, name)
            else -> emptyList()
        }
    }
}
