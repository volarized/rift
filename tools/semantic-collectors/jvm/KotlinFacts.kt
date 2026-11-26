import com.intellij.psi.PsiElement
import java.io.OutputStream
import java.io.PrintStream
import java.nio.file.Files
import java.nio.file.Path
import org.jetbrains.kotlin.KtFakeSourceElementKind
import org.jetbrains.kotlin.KtPsiSourceElement
import org.jetbrains.kotlin.KtRealSourceElementKind
import org.jetbrains.kotlin.KtSourceElement
import org.jetbrains.kotlin.cli.common.ExitCode
import org.jetbrains.kotlin.cli.jvm.K2JVMCompiler
import org.jetbrains.kotlin.compiler.plugin.CompilerPluginRegistrar
import org.jetbrains.kotlin.compiler.plugin.ExperimentalCompilerApi
import org.jetbrains.kotlin.config.CompilerConfiguration
import org.jetbrains.kotlin.diagnostics.DiagnosticReporter
import org.jetbrains.kotlin.fir.FirSession
import org.jetbrains.kotlin.fir.analysis.checkers.MppCheckerKind
import org.jetbrains.kotlin.fir.analysis.checkers.context.CheckerContext
import org.jetbrains.kotlin.fir.analysis.checkers.declaration.DeclarationCheckers
import org.jetbrains.kotlin.fir.analysis.checkers.declaration.FirFileChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.ExpressionCheckers
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirQualifiedAccessExpressionChecker
import org.jetbrains.kotlin.fir.analysis.checkers.expression.FirResolvedQualifierChecker
import org.jetbrains.kotlin.fir.analysis.checkers.type.FirResolvedTypeRefChecker
import org.jetbrains.kotlin.fir.analysis.checkers.type.TypeCheckers
import org.jetbrains.kotlin.fir.analysis.extensions.FirAdditionalCheckersExtension
import org.jetbrains.kotlin.fir.declarations.FirFile
import org.jetbrains.kotlin.fir.declarations.FirResolvedImport
import org.jetbrains.kotlin.fir.expressions.FirFunctionCall
import org.jetbrains.kotlin.fir.expressions.FirQualifiedAccessExpression
import org.jetbrains.kotlin.fir.expressions.FirResolvedQualifier
import org.jetbrains.kotlin.fir.extensions.FirExtensionRegistrar
import org.jetbrains.kotlin.fir.extensions.FirExtensionRegistrarAdapter
import org.jetbrains.kotlin.fir.references.FirResolvedNamedReference
import org.jetbrains.kotlin.fir.resolve.providers.symbolProvider
import org.jetbrains.kotlin.fir.symbols.FirBasedSymbol
import org.jetbrains.kotlin.fir.symbols.impl.FirRegularClassSymbol
import org.jetbrains.kotlin.fir.types.ConeClassLikeType
import org.jetbrains.kotlin.fir.types.ConeTypeParameterType
import org.jetbrains.kotlin.fir.types.FirResolvedTypeRef
import org.jetbrains.kotlin.name.ClassId
import org.jetbrains.kotlin.psi.KtDotQualifiedExpression
import org.jetbrains.kotlin.psi.KtImportDirective
import org.jetbrains.kotlin.psi.KtNameReferenceExpression
import org.jetbrains.kotlin.psi.KtParameter
import org.jetbrains.kotlin.psi.KtTypeReference
import org.jetbrains.kotlin.psi.KtUserType

object KotlinFacts {
    @JvmStatic
    fun main(args: Array<String>) {
        val files = Files.readAllLines(Path.of(args[6]))
        val sources = files.map(Path::of)
        val facts = Facts(Facts.Category.valueOf(args[0]), args[1].toInt(), sources)
        val options = mutableListOf(
            "-kotlin-home", args[3], "-d", args[4],
            "-Xuse-fir-lt=false", "-Xdisable-default-scripting-plugin",
            "-Xplugin=${Path.of(KotlinFacts::class.java.protectionDomain.codeSource.location.toURI())}",
        )
        if (args[2].isNotEmpty()) options.addAll(listOf("-classpath", args[2]))
        if (args[5].toBooleanStrict()) {
            val stdlib = Path.of(args[3], "lib", "kotlin-stdlib.jar")
            options.addAll(listOf("-Xallow-kotlin-package", "-Xfriend-paths=$stdlib"))
        }
        options.addAll(files)

        // The command accepts one selected source. The compiler loads the registrar from this
        // collector's own jar and obtains the output sink from the same class loader.
        KotlinFactsRegistrar.facts = facts
        PrintStream(OutputStream.nullOutputStream()).use { diagnostics ->
            val exit = K2JVMCompiler().exec(diagnostics, *options.toTypedArray())
            facts.partial = facts.partial || exit != ExitCode.OK
        }
        facts.write(System.out)
    }
}

@OptIn(ExperimentalCompilerApi::class)
class KotlinFactsRegistrar : CompilerPluginRegistrar() {
    override val supportsK2 = true

    override fun ExtensionStorage.registerExtensions(configuration: CompilerConfiguration) {
        FirExtensionRegistrarAdapter.registerExtension(Registrar(facts))
    }

    private class Registrar(val facts: Facts) : FirExtensionRegistrar() {
        override fun ExtensionRegistrarContext.configurePlugin() {
            +{ session: FirSession -> KotlinCheckers(session, facts) }
        }
    }

    companion object {
        internal lateinit var facts: Facts
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
        val source = this?.source as? KtPsiSourceElement ?: return null
        val file = source.psi.containingFile?.virtualFile ?: return null
        val id = facts.source(Path.of(file.path)) ?: return null
        val range = if (source.kind == KtFakeSourceElementKind.PropertyFromParameter) {
            // A property declared with val or var in the constructor shares the parameter's name.
            (source.psi as? KtParameter)?.takeIf { it.hasValOrVar() }?.nameIdentifier.range()
        } else {
            source.range()
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
        if (reference?.kind != KtFakeSourceElementKind.ReferenceInAtomicQualifiedAccess) return null

        // FIR gives an authored atomic name a fake reference range and a real expression range.
        val expression = source as? KtPsiSourceElement ?: return null
        return expression.takeIf { it.psi is KtNameReferenceExpression }.range()
    }

    override val expressionCheckers = object : ExpressionCheckers() {
        override val resolvedQualifierCheckers = setOf(
            object : FirResolvedQualifierChecker(MppCheckerKind.Common) {
                context(context: CheckerContext, reporter: DiagnosticReporter)
                override fun check(expression: FirResolvedQualifier) {
                    if (facts.category != Facts.Category.REFERENCES || !context.isSelected()) return
                    val classifier = expression.symbol ?: return
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
                    val target = (reference as? FirResolvedNamedReference)?.resolvedSymbol.anchor()
                    facts.add(occurrence, context.enclosing(), target)
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
